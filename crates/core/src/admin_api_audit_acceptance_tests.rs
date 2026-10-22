//! Acceptance: admin API gateway audit + confirm store (spec §8 slice 2).
//!
//! BUILDER CONTRACT (TOG-36202): this file is frozen. Build on the acceptance
//! card's branch, keep every test here verbatim, and make the suite green by
//! changing production code only: add the cutover migration(s) for
//! `admin_api_audit` + `admin_confirms` + the four `internal_action_log`
//! attribution columns, plus the store functions with their own unit tests.
//! Do not edit the fixtures, the claim SQL, or the assertions: they are the
//! spec. Owner-approved 2026-10-21 ([TOG-35921] spec §4.4, decisions A3/A5/A6).
//!
//! The contract under test:
//!
//! - RED: `admin_api_audit` carries one row per authenticated admin call —
//!   allowed, denied and dry-run alike — with token/scope attribution. The
//!   decision vocabulary is closed (`allowed/denied/dry_run/confirmed`), the
//!   scope vocabulary is closed (`read/moderate/admin`), `token_id` keeps the
//!   spec `adm_` + 16-hex shape, and `guild_id` keeps the repo snowflake
//!   shape (`TEXT` + `^[0-9]{17,20}$`, matching `internal_action_log` and
//!   50+ tables; the token registry's `guild_ids[]` element type is slice 0's
//!   decision and is bridged in code, not here).
//! - RED: `params_redacted` (JSONB, always present) never lands a
//!   secret-bearing top-level key. The database CHECK below is the backstop;
//!   the builder's Rust `redact_*` allowlist (default-deny, case-sensitive,
//!   nesting-aware) plus its unit tests are the primary control and are
//!   required by this contract even though this suite cannot call a function
//!   that does not exist yet. Nested-object redaction is the Rust allowlist's
//!   job and is pinned there, not here.
//! - RED: `admin_confirms` claims are single-use (second claim returns zero
//!   rows, including under a concurrent double-claim race), refuse expired
//!   rows, and refuse any token/guild/verb/plan-hash mismatch without
//!   consuming the row. `params_hash` is stored for forensics and is NOT part
//!   of the claim binding (decisions A3). Minting sets
//!   `expires_at = now + 5 min`; the builder's unit tests pin the mint side,
//!   this suite pins the enforcement side (live claims land, expired refuse).
//! - RED: `internal_action_log` gains nullable `token_id`, `scope`,
//!   `dry_run`, `confirm_id` so execution receipts carry attribution. Existing
//!   phases and vocabularies are frozen: the unattributed control below stays
//!   green throughout.
//! - RED: audit retention is a `created_at`-bounded purge with a 365-day cap,
//!   matching the [`crate::rsvp::AUDIT_RETENTION_DAYS`] precedent. This suite
//!   pins the mechanism (defaulted `created_at`, old rows purgeable, recent
//!   rows survive); the builder adds the named production constant, the purge
//!   path, and unit tests.
//!
//! Deliberately unpinned (builder's choice, review verifies):
//!
//! - Foreign keys. `receipt` links `internal_action_log.intent_id`, which is
//!   not unique (one row per phase), so no FK to the log is possible; the
//!   link is logical. No FK to `admin_api_tokens` is pinned either: denied
//!   and revoked-token audits must land even when no registry row exists, and
//!   the registry ships on slice 0's schedule. The confirmed-row tests mint
//!   their `confirm_id` first so they pass with or without a confirms FK.
//! - Verb vocabulary (open `TEXT`: verbs land in slices 4–7), `confirm_id`
//!   format (opaque `TEXT` PK), extra columns beyond the required sets, index
//!   shapes, and the audit PK column name (a PK must exist).
//! - `dry_run`/`decision` cross-column consistency (e.g. a denied dry-run):
//!   both columns are pinned independently and no combination is forbidden.
//!
//! Suite map (all DB-backed against a disposable `agent-testdb` fixture
//! migrated with the cutover chain; no live Discord; skipped without
//! `TWO_TEST_DATABASE_URL`, which CI always supplies):
//!
//! - controls (green now): fixture smoke over the existing log table;
//!   unattributed log insert still lands (frozen behavior).
//! - schema pins (RED now: relations do not exist): required columns, types,
//!   nullability, and PKs for both new tables plus the four log columns.
//! - audit behavior (RED now): allowed/denied/dry-run/confirmed rows with
//!   attribution; closed decision and scope vocabularies; secret-key backstop
//!   with a structural-params positive; token/guild shape fences; nullable
//!   bigint receipt; defaulted `created_at`; 365-day purge mechanism.
//! - confirm behavior (RED now): single-use incl. concurrent race, expiry
//!   refusal, four-way binding mismatch refusal without consumption, PK
//!   uniqueness, sha256-hex hash fences, token shape fence, 5-minute-horizon
//!   boundary claim.
//!
//! [TOG-35921]: https://github.com/TogetherWeOwn/two-bot-next

#![cfg(test)]

use serde_json::json;
use sqlx::PgPool;
use two_bot_testsupport::TestDatabase;

/// 18-digit snowflake shape; never a real guild.
const GUILD: &str = "123456789012345678";
/// Second snowflake for mismatch probes; never a real guild.
const OTHER_GUILD: &str = "223456789012345678";

/// `adm_` + 16 lowercase hex per spec §4.1; the trailing nibble varies per row.
fn token_id(nibble: char) -> String {
    format!("adm_{nibble}123456789abcdef")
}

/// 64 lowercase hex (`body_hash` shape); distinct per seed.
fn hex64(seed: u8) -> String {
    format!("{seed:02x}").repeat(32)
}

/// Only an absent URL skips the test. Configured setup failures are errors.
async fn fixture() -> Option<TestDatabase> {
    let url = match std::env::var("TWO_TEST_DATABASE_URL") {
        Ok(url) => url,
        Err(std::env::VarError::NotPresent) => {
            assert!(
                std::env::var("GITHUB_ACTIONS").is_err(),
                "CI must supply the guarded test database"
            );
            eprintln!("SKIP admin audit acceptance: TWO_TEST_DATABASE_URL is not set");
            return None;
        }
        Err(_) => panic!("TWO_TEST_DATABASE_URL must be Unicode"),
    };
    Some(
        TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("create migrated agent-testdb fixture"),
    )
}

/// `(column_name, data_type, is_nullable)` for one table in `public`.
async fn columns(pool: &PgPool, table: &str) -> Vec<(String, String, String)> {
    sqlx::query_as(
        "SELECT column_name, data_type, is_nullable \
         FROM information_schema.columns \
         WHERE table_schema = 'public' AND table_name = $1",
    )
    .bind(table)
    .fetch_all(pool)
    .await
    .expect("read information_schema columns")
}

/// Assert every `(name, type, nullable)` triple is present. Extra columns are
/// allowed (the builder may add e.g. `created_at` to confirms); missing or
/// mistyped required columns fail.
fn assert_columns(
    actual: &[(String, String, String)],
    table: &str,
    expected: &[(&str, &str, &str)],
) {
    for (name, data_type, nullable) in expected {
        let found = actual.iter().find(|(column, _, _)| column == name);
        let Some((_, actual_type, actual_nullable)) = found else {
            panic!("{table} is missing the {name} column; has {actual:?}");
        };
        assert_eq!(
            actual_type, data_type,
            "{table}.{name} must be {data_type}, got {actual_type}"
        );
        assert_eq!(
            actual_nullable, nullable,
            "{table}.{name} nullability must be {nullable}, got {actual_nullable}"
        );
    }
}

/// Primary-key columns for one table, ordinal order.
async fn primary_key_columns(pool: &PgPool, table: &str) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT kcu.column_name \
         FROM information_schema.table_constraints tc \
         JOIN information_schema.key_column_usage kcu \
           ON tc.constraint_name = kcu.constraint_name \
          AND tc.table_schema = kcu.table_schema \
         WHERE tc.table_schema = 'public' AND tc.table_name = $1 \
           AND tc.constraint_type = 'PRIMARY KEY' \
         ORDER BY kcu.ordinal_position",
    )
    .bind(table)
    .fetch_all(pool)
    .await
    .expect("read primary key columns")
}

/// Postgres `check_violation` (23514). On the baseline the same statement
/// fails with `undefined_table` (42P01) instead, which fails this assertion
/// with the missing relation named — RED for the right reason.
fn assert_check_violation<T>(result: Result<T, sqlx::Error>, what: &str) {
    let Err(error) = result else {
        panic!("{what}: the forbidden row landed; a CHECK must refuse it");
    };
    let code = error
        .as_database_error()
        .and_then(|error| error.code())
        .map(|code| code.to_string());
    assert_eq!(
        code.as_deref(),
        Some("23514"),
        "{what}: expected a CHECK violation (23514), got {code:?}: {error}"
    );
}

/// Postgres `unique_violation` (23505).
fn assert_unique_violation<T>(result: Result<T, sqlx::Error>, what: &str) {
    let Err(error) = result else {
        panic!("{what}: the duplicate row landed; a UNIQUE/PK must refuse it");
    };
    let code = error
        .as_database_error()
        .and_then(|error| error.code())
        .map(|code| code.to_string());
    assert_eq!(
        code.as_deref(),
        Some("23505"),
        "{what}: expected a uniqueness violation (23505), got {code:?}: {error}"
    );
}

/// Mint one confirm expiring `expires_in_seconds` from now (negative = already
/// expired). The builder's production mint wraps this shape with
/// `expires_in_seconds = 300` and unit tests pinning that TTL.
#[allow(clippy::too_many_arguments)]
async fn mint(
    pool: &PgPool,
    confirm_id: &str,
    token_id: &str,
    guild_id: &str,
    verb: &str,
    plan_hash: &str,
    params_hash: &str,
    expires_in_seconds: i64,
) {
    sqlx::query(
        "INSERT INTO admin_confirms \
         (confirm_id, token_id, guild_id, verb, plan_hash, params_hash, expires_at) \
         VALUES ($1, $2, $3, $4, $5, $6, clock_timestamp() + ($7 * interval '1 second'))",
    )
    .bind(confirm_id)
    .bind(token_id)
    .bind(guild_id)
    .bind(verb)
    .bind(plan_hash)
    .bind(params_hash)
    .bind(expires_in_seconds)
    .execute(pool)
    .await
    .unwrap_or_else(|error| panic!("mint confirm {confirm_id}: {error}"));
}

/// The claim every builder implementation must execute: one atomic statement,
/// single-use (`used_at IS NULL`), live (`expires_at` in the future), and
/// bound to token + guild + verb + plan hash. Returns the claimed ids
/// (empty = refused). `params_hash` is forensic, never a claim input.
async fn claim(
    pool: &PgPool,
    confirm_id: &str,
    token_id: &str,
    guild_id: &str,
    verb: &str,
    plan_hash: &str,
) -> Vec<String> {
    sqlx::query_scalar(
        "UPDATE admin_confirms SET used_at = clock_timestamp() \
         WHERE confirm_id = $1 AND used_at IS NULL AND expires_at > clock_timestamp() \
           AND token_id = $2 AND guild_id = $3 AND verb = $4 AND plan_hash = $5 \
         RETURNING confirm_id",
    )
    .bind(confirm_id)
    .bind(token_id)
    .bind(guild_id)
    .bind(verb)
    .bind(plan_hash)
    .fetch_all(pool)
    .await
    .unwrap_or_else(|error| panic!("claim confirm {confirm_id}: {error}"))
}

async fn used(pool: &PgPool, confirm_id: &str) -> bool {
    sqlx::query_scalar("SELECT used_at IS NOT NULL FROM admin_confirms WHERE confirm_id = $1")
        .bind(confirm_id)
        .fetch_one(pool)
        .await
        .unwrap_or_else(|error| panic!("read used_at for {confirm_id}: {error}"))
}

/// Minimal `internal_idempotency` parent for log-row tests. `key_hash`
/// distinguishes parents within one fixture database.
async fn seed_intent(pool: &PgPool, key_hash: &str) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO internal_idempotency \
         (caller_hash, key_hash, action, payload_hash, state, guild_id) \
         VALUES ($1, $2, 'moderation.warn', $3, 'in_flight', $4) \
         RETURNING intent_id",
    )
    .bind(hex64(1))
    .bind(key_hash)
    .bind(hex64(2))
    .bind(GUILD)
    .fetch_one(pool)
    .await
    .expect("seed internal_idempotency parent")
}

// Control (green now): the fixture migrates and the existing log table keeps
// its shape. If this fails, the harness — not the slice — is broken.
#[tokio::test]
async fn control_fixture_migrates_existing_tables() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let pool = fixture.pool();
    let actual = columns(pool, "internal_action_log").await;
    assert_columns(
        &actual,
        "internal_action_log",
        &[
            ("audit_id", "bigint", "NO"),
            ("intent_id", "bigint", "NO"),
            ("phase", "text", "NO"),
        ],
    );
    assert_eq!(
        primary_key_columns(pool, "internal_action_log").await,
        ["audit_id"],
        "existing log PK is untouched"
    );
    fixture.close().await.unwrap();
}

// Control (green now): unattributed log inserts keep landing exactly as
// today. The builder's four new columns are nullable, so this stays green.
#[tokio::test]
async fn control_unattributed_log_insert_still_lands() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let pool = fixture.pool();
    let intent_id = seed_intent(pool, &hex64(3)).await;
    sqlx::query(
        "INSERT INTO internal_action_log (intent_id, phase, caller_hash, action) \
         VALUES ($1, 'terminal', $2, 'moderation.warn')",
    )
    .bind(intent_id)
    .bind(hex64(1))
    .execute(pool)
    .await
    .expect("unattributed log insert lands");
    // Existing columns only: this SELECT is identical pre- and post-slice,
    // so the control is green in both states.
    let phases: Vec<String> =
        sqlx::query_scalar("SELECT phase FROM internal_action_log WHERE intent_id = $1")
            .bind(intent_id)
            .fetch_all(pool)
            .await
            .expect("read log row back");
    assert_eq!(phases, ["terminal"]);
    fixture.close().await.unwrap();
}

// RED now: the audit table with its ten spec columns, types and nullability.
#[tokio::test]
async fn audit_table_has_the_spec_columns() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let pool = fixture.pool();
    let actual = columns(pool, "admin_api_audit").await;
    assert_columns(
        &actual,
        "admin_api_audit",
        &[
            ("token_id", "text", "NO"),
            ("scope", "text", "NO"),
            ("guild_id", "text", "NO"),
            ("verb", "text", "NO"),
            ("params_redacted", "jsonb", "NO"),
            ("dry_run", "boolean", "NO"),
            ("confirm_id", "text", "YES"),
            ("decision", "text", "NO"),
            ("receipt", "bigint", "YES"),
            ("created_at", "timestamp with time zone", "NO"),
        ],
    );
    assert!(
        !primary_key_columns(pool, "admin_api_audit")
            .await
            .is_empty(),
        "the audit table must have a primary key, got {actual:?}"
    );
    fixture.close().await.unwrap();
}

// RED now: the confirms table with its eight spec columns plus the PK.
#[tokio::test]
async fn confirms_table_has_the_spec_columns() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let pool = fixture.pool();
    let actual = columns(pool, "admin_confirms").await;
    assert_columns(
        &actual,
        "admin_confirms",
        &[
            ("confirm_id", "text", "NO"),
            ("token_id", "text", "NO"),
            ("guild_id", "text", "NO"),
            ("verb", "text", "NO"),
            ("plan_hash", "text", "NO"),
            ("params_hash", "text", "NO"),
            ("expires_at", "timestamp with time zone", "NO"),
            ("used_at", "timestamp with time zone", "YES"),
        ],
    );
    assert_eq!(
        primary_key_columns(pool, "admin_confirms").await,
        ["confirm_id"],
        "confirm_id is the primary key"
    );
    fixture.close().await.unwrap();
}

// RED now: the log carries four nullable attribution columns.
#[tokio::test]
async fn internal_action_log_carries_nullable_admin_attribution() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let pool = fixture.pool();
    let actual = columns(pool, "internal_action_log").await;
    assert_columns(
        &actual,
        "internal_action_log",
        &[
            ("token_id", "text", "YES"),
            ("scope", "text", "YES"),
            ("dry_run", "boolean", "YES"),
            ("confirm_id", "text", "YES"),
        ],
    );
    fixture.close().await.unwrap();
}

/// One audit row write through the builder's column contract.
#[allow(clippy::too_many_arguments)]
async fn write_audit(
    pool: &PgPool,
    token_id: &str,
    scope: &str,
    guild_id: &str,
    verb: &str,
    params: &serde_json::Value,
    dry_run: bool,
    confirm_id: Option<&str>,
    decision: &str,
    receipt: Option<i64>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO admin_api_audit \
         (token_id, scope, guild_id, verb, params_redacted, dry_run, confirm_id, decision, receipt) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(token_id)
    .bind(scope)
    .bind(guild_id)
    .bind(verb)
    .bind(sqlx::types::Json(params))
    .bind(dry_run)
    .bind(confirm_id)
    .bind(decision)
    .bind(receipt)
    .execute(pool)
    .await
    .map(|_| ())
}

// RED now: allowed + denied + dry-run + confirmed rows land with exact
// token/scope attribution. The confirmed row references a minted confirm so
// the test passes with or without a confirms FK.
#[tokio::test]
async fn audit_rows_land_for_allowed_denied_and_dry_run_with_attribution() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let pool = fixture.pool();
    mint(
        pool,
        "cfm_audit_confirmed_01",
        &token_id('a'),
        GUILD,
        "moderation.ban",
        &hex64(10),
        &hex64(11),
        300,
    )
    .await;
    write_audit(
        pool,
        &token_id('1'),
        "moderate",
        GUILD,
        "moderation.warn",
        &json!({"limit": 25}),
        false,
        None,
        "allowed",
        None,
    )
    .await
    .expect("allowed audit row lands");
    write_audit(
        pool,
        &token_id('2'),
        "read",
        GUILD,
        "channel.create",
        &json!({"limit": 1}),
        false,
        None,
        "denied",
        None,
    )
    .await
    .expect("denied audit row lands");
    write_audit(
        pool,
        &token_id('3'),
        "admin",
        GUILD,
        "moderation.ban",
        &json!({"dry_run": true}),
        true,
        None,
        "dry_run",
        None,
    )
    .await
    .expect("dry-run audit row lands");
    write_audit(
        pool,
        &token_id('a'),
        "admin",
        GUILD,
        "moderation.ban",
        &json!({}),
        false,
        Some("cfm_audit_confirmed_01"),
        "confirmed",
        None,
    )
    .await
    .expect("confirmed audit row lands");
    let mut rows: Vec<(String, bool, String, String, Option<String>)> = sqlx::query_as(
        "SELECT decision, dry_run, token_id, scope, confirm_id FROM admin_api_audit",
    )
    .fetch_all(pool)
    .await
    .expect("read audit rows back");
    rows.sort();
    assert_eq!(
        rows,
        [
            (
                "allowed".to_owned(),
                false,
                token_id('1'),
                "moderate".to_owned(),
                None
            ),
            (
                "confirmed".to_owned(),
                false,
                token_id('a'),
                "admin".to_owned(),
                Some("cfm_audit_confirmed_01".to_owned())
            ),
            (
                "denied".to_owned(),
                false,
                token_id('2'),
                "read".to_owned(),
                None
            ),
            (
                "dry_run".to_owned(),
                true,
                token_id('3'),
                "admin".to_owned(),
                None
            ),
        ],
        "all four decisions land with exact attribution"
    );
    fixture.close().await.unwrap();
}

// RED now: only the four spec decisions land; anything else is refused.
#[tokio::test]
async fn audit_decision_vocabulary_is_closed() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let pool = fixture.pool();
    for decision in ["maybe", "ALLOWED", ""] {
        let result = write_audit(
            pool,
            &token_id('b'),
            "read",
            GUILD,
            "guild.read",
            &json!({}),
            false,
            None,
            decision,
            None,
        )
        .await;
        assert_check_violation(result, &format!("decision {decision:?}"));
    }
    fixture.close().await.unwrap();
}

// RED now: the three scope tiers land; anything else is refused.
#[tokio::test]
async fn audit_scope_vocabulary_is_closed() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let pool = fixture.pool();
    for (nibble, scope) in [('c', "read"), ('d', "moderate"), ('e', "admin")] {
        write_audit(
            pool,
            &token_id(nibble),
            scope,
            GUILD,
            "guild.read",
            &json!({}),
            false,
            None,
            "allowed",
            None,
        )
        .await
        .unwrap_or_else(|error| panic!("scope {scope} lands: {error}"));
    }
    for scope in ["superuser", "ADMIN", ""] {
        let result = write_audit(
            pool,
            &token_id('f'),
            scope,
            GUILD,
            "guild.read",
            &json!({}),
            false,
            None,
            "allowed",
            None,
        )
        .await;
        assert_check_violation(result, &format!("scope {scope:?}"));
    }
    fixture.close().await.unwrap();
}

// RED now: a secret-bearing top-level params key never lands. Positive
// structural params land. Nested redaction is the Rust allowlist's job.
#[tokio::test]
async fn audit_params_redacted_rejects_secret_bearing_keys() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let pool = fixture.pool();
    write_audit(
        pool,
        &token_id('0'),
        "read",
        GUILD,
        "guild.read",
        &json!({"limit": 25, "guild_id": GUILD, "dry_run": true}),
        false,
        None,
        "allowed",
        None,
    )
    .await
    .expect("structural params land");
    for key in [
        "secret",
        "secrets",
        "token",
        "access_token",
        "refresh_token",
        "bearer",
        "authorization",
        "password",
        "passwd",
        "api_key",
        "apikey",
        "private_key",
        "client_secret",
        "session_token",
    ] {
        // Explicit map: `json!` with a variable key would stringify the
        // identifier instead of using its value.
        let mut object = serde_json::Map::new();
        object.insert(key.to_owned(), json!("fixture-not-a-secret"));
        object.insert("limit".to_owned(), json!(1));
        let hostile = serde_json::Value::Object(object);
        let result = write_audit(
            pool,
            &token_id('0'),
            "read",
            GUILD,
            "guild.read",
            &hostile,
            false,
            None,
            "allowed",
            None,
        )
        .await;
        assert_check_violation(result, &format!("params key {key:?}"));
    }
    fixture.close().await.unwrap();
}

// RED now: audit token ids keep the spec `adm_` + 16-hex shape.
#[tokio::test]
async fn audit_token_id_shape_is_pinned() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let pool = fixture.pool();
    for token in [
        "not-a-token",
        "adm_short",
        "adm_0123456789abcdefg",
        "ADM_0123456789abcdef",
        "",
    ] {
        let result = write_audit(
            pool,
            token,
            "read",
            GUILD,
            "guild.read",
            &json!({}),
            false,
            None,
            "allowed",
            None,
        )
        .await;
        assert_check_violation(result, &format!("token_id {token:?}"));
    }
    fixture.close().await.unwrap();
}

// RED now: audit guild ids keep the repo snowflake shape.
#[tokio::test]
async fn audit_guild_id_is_a_snowflake() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let pool = fixture.pool();
    for guild in ["not-a-guild", "123", "123456789012345678901", ""] {
        let result = write_audit(
            pool,
            &token_id('4'),
            "read",
            guild,
            "guild.read",
            &json!({}),
            false,
            None,
            "allowed",
            None,
        )
        .await;
        assert_check_violation(result, &format!("guild_id {guild:?}"));
    }
    fixture.close().await.unwrap();
}

// RED now: the receipt is a nullable bigint linking an intent. NULL lands
// (denied/dry-run rows have no receipt); a live intent id round-trips,
// which also passes if the builder adds an intent FK.
#[tokio::test]
async fn audit_receipt_is_a_nullable_bigint() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let pool = fixture.pool();
    let intent_id = seed_intent(pool, &hex64(4)).await;
    write_audit(
        pool,
        &token_id('5'),
        "moderate",
        GUILD,
        "moderation.warn",
        &json!({}),
        false,
        None,
        "denied",
        None,
    )
    .await
    .expect("NULL receipt lands");
    write_audit(
        pool,
        &token_id('6'),
        "moderate",
        GUILD,
        "moderation.warn",
        &json!({}),
        false,
        None,
        "allowed",
        Some(intent_id),
    )
    .await
    .expect("intent receipt lands");
    let mut receipts: Vec<(String, Option<i64>)> =
        sqlx::query_as("SELECT decision, receipt FROM admin_api_audit")
            .fetch_all(pool)
            .await
            .expect("read receipts back");
    receipts.sort();
    assert_eq!(
        receipts,
        [
            ("allowed".to_owned(), Some(intent_id)),
            ("denied".to_owned(), None)
        ]
    );
    fixture.close().await.unwrap();
}

// RED now: `created_at` defaults to now when the writer omits it.
#[tokio::test]
async fn audit_created_at_defaults_to_now() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let pool = fixture.pool();
    write_audit(
        pool,
        &token_id('7'),
        "read",
        GUILD,
        "guild.read",
        &json!({}),
        false,
        None,
        "allowed",
        None,
    )
    .await
    .expect("audit row without created_at lands");
    let fresh: bool = sqlx::query_scalar(
        "SELECT created_at > clock_timestamp() - interval '5 minutes' \
          AND created_at <= clock_timestamp() + interval '1 minute' \
         FROM admin_api_audit",
    )
    .fetch_one(pool)
    .await
    .expect("read created_at back");
    assert!(fresh, "created_at must default to now");
    fixture.close().await.unwrap();
}

// RED now: the retention mechanism — rows older than the 365-day cap purge,
// recent rows survive. The builder's production purge uses the same horizon
// behind a named constant with unit tests.
#[tokio::test]
async fn audit_retention_purge_keeps_recent_clears_old() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let pool = fixture.pool();
    sqlx::query(
        "INSERT INTO admin_api_audit \
         (token_id, scope, guild_id, verb, params_redacted, dry_run, decision, created_at) \
         VALUES ($1, 'read', $2, 'guild.read', '{}', false, 'allowed', \
                 clock_timestamp() - interval '400 days')",
    )
    .bind(token_id('8'))
    .bind(GUILD)
    .execute(pool)
    .await
    .expect("old audit row seeds");
    write_audit(
        pool,
        &token_id('9'),
        "read",
        GUILD,
        "guild.read",
        &json!({}),
        false,
        None,
        "denied",
        None,
    )
    .await
    .expect("recent audit row seeds");
    let purged = sqlx::query(
        "DELETE FROM admin_api_audit \
         WHERE created_at < clock_timestamp() - interval '365 days'",
    )
    .execute(pool)
    .await
    .expect("retention purge runs");
    assert_eq!(purged.rows_affected(), 1, "exactly the old row purges");
    let survivors: Vec<String> = sqlx::query_scalar("SELECT decision FROM admin_api_audit")
        .fetch_all(pool)
        .await
        .expect("read survivors");
    assert_eq!(survivors, ["denied"], "only the recent row survives");
    fixture.close().await.unwrap();
}

// RED now: an attributed log row round-trips all four new columns. The
// confirm is minted first so the test passes with or without a confirms FK.
#[tokio::test]
async fn internal_action_log_attributed_insert_round_trips() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let pool = fixture.pool();
    mint(
        pool,
        "cfm_log_attributed_01",
        &token_id('b'),
        GUILD,
        "moderation.ban",
        &hex64(12),
        &hex64(13),
        300,
    )
    .await;
    let intent_id = seed_intent(pool, &hex64(5)).await;
    sqlx::query(
        "INSERT INTO internal_action_log \
         (intent_id, phase, caller_hash, action, guild_id, token_id, scope, dry_run, confirm_id) \
         VALUES ($1, 'terminal', $2, 'moderation.warn', $3, $4, 'admin', true, $5)",
    )
    .bind(intent_id)
    .bind(hex64(1))
    .bind(GUILD)
    .bind(token_id('b'))
    .bind("cfm_log_attributed_01")
    .execute(pool)
    .await
    .expect("attributed log insert lands");
    let row: (Option<String>, Option<String>, Option<bool>, Option<String>) = sqlx::query_as(
        "SELECT token_id, scope, dry_run, confirm_id \
         FROM internal_action_log WHERE intent_id = $1",
    )
    .bind(intent_id)
    .fetch_one(pool)
    .await
    .expect("read attribution back");
    assert_eq!(
        row,
        (
            Some(token_id('b')),
            Some("admin".to_owned()),
            Some(true),
            Some("cfm_log_attributed_01".to_owned())
        ),
        "all four attribution columns round-trip"
    );
    fixture.close().await.unwrap();
}

// RED now: the log's scope keeps the three-tier vocabulary, and its token
// ids keep the `adm_` shape — matching the table's existing CHECK discipline.
#[tokio::test]
async fn internal_action_log_attribution_vocabulary_is_closed() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let pool = fixture.pool();
    for scope in ["superuser", "ADMIN", ""] {
        let intent_id = seed_intent(pool, &hex64(20 + (scope.len() as u8))).await;
        let result = sqlx::query(
            "INSERT INTO internal_action_log \
             (intent_id, phase, caller_hash, action, scope) \
             VALUES ($1, 'terminal', $2, 'moderation.warn', $3)",
        )
        .bind(intent_id)
        .bind(hex64(1))
        .bind(scope)
        .execute(pool)
        .await
        .map(|_| ());
        assert_check_violation(result, &format!("log scope {scope:?}"));
    }
    let intent_id = seed_intent(pool, &hex64(40)).await;
    let result = sqlx::query(
        "INSERT INTO internal_action_log \
         (intent_id, phase, caller_hash, action, token_id) \
         VALUES ($1, 'terminal', $2, 'moderation.warn', 'not-a-token')",
    )
    .bind(intent_id)
    .bind(hex64(1))
    .execute(pool)
    .await
    .map(|_| ());
    assert_check_violation(result, "log token_id shape");
    fixture.close().await.unwrap();
}

// RED now: a live claim lands once; the repeat claims nothing. A concurrent
// double-claim race still yields exactly one winner: the claim is one atomic
// statement, never check-then-set.
#[tokio::test]
async fn confirm_claim_is_single_use() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let pool = fixture.pool();
    let token = token_id('c');
    mint(
        pool,
        "cfm_single_use_01",
        &token,
        GUILD,
        "moderation.ban",
        &hex64(30),
        &hex64(31),
        300,
    )
    .await;
    assert_eq!(
        claim(
            pool,
            "cfm_single_use_01",
            &token,
            GUILD,
            "moderation.ban",
            &hex64(30)
        )
        .await,
        ["cfm_single_use_01"],
        "the first live claim lands"
    );
    assert!(used(pool, "cfm_single_use_01").await, "used_at is set");
    assert!(
        claim(
            pool,
            "cfm_single_use_01",
            &token,
            GUILD,
            "moderation.ban",
            &hex64(30)
        )
        .await
        .is_empty(),
        "the repeat claim refuses"
    );
    mint(
        pool,
        "cfm_single_use_race",
        &token,
        GUILD,
        "moderation.ban",
        &hex64(32),
        &hex64(33),
        300,
    )
    .await;
    // Hoisted: `join!` binds both futures before polling, so a temporary
    // hash would drop while still borrowed.
    let race_hash = hex64(32);
    let (first, second) = tokio::join!(
        claim(
            pool,
            "cfm_single_use_race",
            &token,
            GUILD,
            "moderation.ban",
            &race_hash
        ),
        claim(
            pool,
            "cfm_single_use_race",
            &token,
            GUILD,
            "moderation.ban",
            &race_hash
        )
    );
    assert_eq!(
        first.len() + second.len(),
        1,
        "a concurrent double-claim yields exactly one winner"
    );
    fixture.close().await.unwrap();
}

// RED now: an expired confirm refuses and stays unused.
#[tokio::test]
async fn confirm_claim_refuses_expired() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let pool = fixture.pool();
    let token = token_id('d');
    mint(
        pool,
        "cfm_expired_01",
        &token,
        GUILD,
        "moderation.ban",
        &hex64(34),
        &hex64(35),
        -60,
    )
    .await;
    assert!(
        claim(
            pool,
            "cfm_expired_01",
            &token,
            GUILD,
            "moderation.ban",
            &hex64(34)
        )
        .await
        .is_empty(),
        "the expired claim refuses"
    );
    assert!(
        !used(pool, "cfm_expired_01").await,
        "the refused row stays unused"
    );
    fixture.close().await.unwrap();
}

// RED now: any binding mismatch refuses without consuming; the correct claim
// then still lands, proving the mismatches were side-effect free.
#[tokio::test]
async fn confirm_claim_refuses_binding_mismatch() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let pool = fixture.pool();
    let token = token_id('e');
    mint(
        pool,
        "cfm_bound_01",
        &token,
        GUILD,
        "moderation.ban",
        &hex64(36),
        &hex64(37),
        300,
    )
    .await;
    assert!(
        claim(
            pool,
            "cfm_bound_01",
            &token_id('f'),
            GUILD,
            "moderation.ban",
            &hex64(36)
        )
        .await
        .is_empty(),
        "wrong token refuses"
    );
    assert!(
        claim(
            pool,
            "cfm_bound_01",
            &token,
            OTHER_GUILD,
            "moderation.ban",
            &hex64(36)
        )
        .await
        .is_empty(),
        "wrong guild refuses"
    );
    assert!(
        claim(
            pool,
            "cfm_bound_01",
            &token,
            GUILD,
            "moderation.kick",
            &hex64(36)
        )
        .await
        .is_empty(),
        "wrong verb refuses"
    );
    assert!(
        claim(
            pool,
            "cfm_bound_01",
            &token,
            GUILD,
            "moderation.ban",
            &hex64(38)
        )
        .await
        .is_empty(),
        "wrong plan hash refuses"
    );
    assert!(
        !used(pool, "cfm_bound_01").await,
        "mismatches never consume the row"
    );
    assert_eq!(
        claim(
            pool,
            "cfm_bound_01",
            &token,
            GUILD,
            "moderation.ban",
            &hex64(36)
        )
        .await,
        ["cfm_bound_01"],
        "the correct claim still lands after mismatches"
    );
    fixture.close().await.unwrap();
}

// RED now: `confirm_id` is unique — the second mint with one id refuses.
#[tokio::test]
async fn confirm_id_is_unique() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let pool = fixture.pool();
    mint(
        pool,
        "cfm_unique_01",
        &token_id('0'),
        GUILD,
        "moderation.ban",
        &hex64(50),
        &hex64(51),
        300,
    )
    .await;
    let result = sqlx::query(
        "INSERT INTO admin_confirms \
         (confirm_id, token_id, guild_id, verb, plan_hash, params_hash, expires_at) \
         VALUES ('cfm_unique_01', $1, $2, 'moderation.ban', $3, $4, \
                 clock_timestamp() + interval '5 minutes')",
    )
    .bind(token_id('0'))
    .bind(GUILD)
    .bind(hex64(50))
    .bind(hex64(51))
    .execute(pool)
    .await
    .map(|_| ());
    assert_unique_violation(result, "duplicate confirm_id");
    fixture.close().await.unwrap();
}

// RED now: plan and params hashes keep the 64-hex `body_hash` shape,
// matching the caller/key/payload hash fences.
#[tokio::test]
async fn confirm_hashes_are_sha256_hex() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let pool = fixture.pool();
    let bad_hashes = [
        "not-a-hash".to_owned(),
        "a".repeat(63),
        "z".repeat(64),
        String::new(),
    ];
    for (index, bad) in bad_hashes.iter().enumerate() {
        // Distinct ids per probe: a missing CHECK must surface as a landed
        // row (23514 assertion), never as a confusing duplicate-key error.
        let plan_result = sqlx::query(
            "INSERT INTO admin_confirms \
             (confirm_id, token_id, guild_id, verb, plan_hash, params_hash, expires_at) \
             VALUES ($1, $2, $3, 'moderation.ban', $4, $5, \
                     clock_timestamp() + interval '5 minutes')",
        )
        .bind(format!("cfm_hash_plan_{index}"))
        .bind(token_id('1'))
        .bind(GUILD)
        .bind(bad)
        .bind(hex64(52))
        .execute(pool)
        .await
        .map(|_| ());
        assert_check_violation(plan_result, &format!("plan_hash {bad:?}"));
        let params_result = sqlx::query(
            "INSERT INTO admin_confirms \
             (confirm_id, token_id, guild_id, verb, plan_hash, params_hash, expires_at) \
             VALUES ($1, $2, $3, 'moderation.ban', $4, $5, \
                     clock_timestamp() + interval '5 minutes')",
        )
        .bind(format!("cfm_hash_params_{index}"))
        .bind(token_id('1'))
        .bind(GUILD)
        .bind(hex64(52))
        .bind(bad)
        .execute(pool)
        .await
        .map(|_| ());
        assert_check_violation(params_result, &format!("params_hash {bad:?}"));
    }
    fixture.close().await.unwrap();
}

// RED now: confirm token ids keep the spec `adm_` + 16-hex shape, and guild
// ids keep the repo snowflake shape.
#[tokio::test]
async fn confirm_attribution_shapes_are_pinned() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let pool = fixture.pool();
    for (index, token) in ["not-a-token", "adm_short", ""].into_iter().enumerate() {
        let result = sqlx::query(
            "INSERT INTO admin_confirms \
             (confirm_id, token_id, guild_id, verb, plan_hash, params_hash, expires_at) \
             VALUES ($1, $2, $3, 'moderation.ban', $4, $5, \
                     clock_timestamp() + interval '5 minutes')",
        )
        .bind(format!("cfm_token_shape_{index}"))
        .bind(token)
        .bind(GUILD)
        .bind(hex64(53))
        .bind(hex64(54))
        .execute(pool)
        .await
        .map(|_| ());
        assert_check_violation(result, &format!("confirms token_id {token:?}"));
    }
    let result = sqlx::query(
        "INSERT INTO admin_confirms \
         (confirm_id, token_id, guild_id, verb, plan_hash, params_hash, expires_at) \
         VALUES ('cfm_guild_shape', $1, 'not-a-guild', 'moderation.ban', $2, $3, \
                 clock_timestamp() + interval '5 minutes')",
    )
    .bind(token_id('2'))
    .bind(hex64(53))
    .bind(hex64(54))
    .execute(pool)
    .await
    .map(|_| ());
    assert_check_violation(result, "confirms guild_id shape");
    fixture.close().await.unwrap();
}

// RED now: a confirm inside the 5-minute horizon claims. The mint side
// (`expires_at = now + 300 s`) is the builder's unit-tested responsibility;
// this pins the enforcement horizon shape from the store side.
#[tokio::test]
async fn live_confirm_inside_five_minute_horizon_claims() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let pool = fixture.pool();
    let token = token_id('3');
    mint(
        pool,
        "cfm_horizon_01",
        &token,
        GUILD,
        "moderation.ban",
        &hex64(55),
        &hex64(56),
        299,
    )
    .await;
    assert_eq!(
        claim(
            pool,
            "cfm_horizon_01",
            &token,
            GUILD,
            "moderation.ban",
            &hex64(55)
        )
        .await,
        ["cfm_horizon_01"],
        "a confirm 299 s from expiry claims"
    );
    fixture.close().await.unwrap();
}
