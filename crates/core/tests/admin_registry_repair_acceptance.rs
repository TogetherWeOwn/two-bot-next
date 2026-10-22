//! Protected acceptance: named admin holders, finite 90-day lifetime and safe upgrades.
//!
//! Test Author owns this file. Builders must not change these assertions.
//! The shipped registry migrations are immutable; repairs must be forward-only
//! in BOTH chains. No bearer transport, registry lookup, credentials or issuance
//! is implemented here. Gateway bearer parsing belongs to the later gateway slice.
//!
//! Breaks caught: missing/nullable/fabricated holder authority, AI admin grants,
//! unbounded/nonfinite lifetimes, store-only fixes and partially applied upgrades.
//! Database fixtures use only the guarded disposable agent-testdb service.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

const CHAINS: [(&str, i64); 2] = [("cutover", 456), ("store", 457)];

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn forward_registry_files(chain: &str, shipped: i64) -> Vec<PathBuf> {
    let mut files: Vec<_> = std::fs::read_dir(root().join(format!("crates/{chain}/migrations")))
        .expect("read migration directory")
        .map(|entry| entry.expect("migration entry").path())
        .filter(|path| {
            path.extension().is_some_and(|ext| ext == "sql")
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .and_then(|name| name.split('_').next())
                    .and_then(|version| version.parse::<i64>().ok())
                    .is_some_and(|version| version > shipped)
                && std::fs::read_to_string(path)
                    .expect("read forward migration")
                    .contains("admin_api_tokens")
        })
        .collect();
    files.sort();
    files
}

#[test]
fn shipped_registry_migrations_are_byte_immutable() {
    // SHA-256 pins are independently recorded from the shipped foundation base.
    for (path, digest) in [
        (
            "crates/cutover/migrations/0456_admin_api_tokens.sql",
            "3452a9adac7ee312a3f10f4150d34effff441e3fac32931a1e8bc08860ac4be2",
        ),
        (
            "crates/store/migrations/0457_admin_api_tokens.sql",
            "622bcbc5cfc2561432290c293e15d30d89997435970088c6e4c01f37e20f962d",
        ),
    ] {
        let bytes = std::fs::read(root().join(path)).expect("read shipped migration");
        assert_eq!(
            hex::encode(Sha256::digest(bytes)),
            digest,
            "{path}: add a forward migration, never rewrite shipped SQL"
        );
    }
}

#[test]
fn both_chains_have_checksum_locked_forward_registry_repairs() {
    let lock: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(root().join("migrations.lock")).expect("read migration lock"),
    )
    .expect("parse migration lock");
    for (chain, shipped) in CHAINS {
        let files = forward_registry_files(chain, shipped);
        assert!(
            !files.is_empty(),
            "{chain}: missing forward admin registry repair after {shipped}"
        );
        for path in files {
            let relative = path.strip_prefix(root()).expect("repository migration");
            let key = relative.to_str().expect("UTF-8 migration path");
            let bytes = std::fs::read(&path).expect("read migration bytes");
            assert_eq!(
                lock["migrations"][key]["sha256"].as_str(),
                Some(hex::encode(Sha256::digest(bytes)).as_str()),
                "{key}: reviewed lock entry must match the forward SQL"
            );
        }
    }
}

#[cfg(feature = "db")]
mod database {
    use serde_json::{json, Value};
    use sqlx::{migrate::Migrator, PgPool};
    use two_bot_testsupport::TestDatabase;

    const TOKEN: &str = "adm_0123456789abcdef";
    const ISSUED: &str = "2026-09-01T00:00:00Z";
    // Exactly 90 days after ISSUED, not a calendar-month approximation.
    const NINETY_DAYS: &str = "2026-11-30T00:00:00Z";

    fn bootstrap() -> Option<String> {
        match std::env::var("TWO_TEST_DATABASE_URL") {
            Ok(url) => Some(url),
            Err(_) => {
                assert_ne!(
                    std::env::var("GITHUB_ACTIONS").as_deref(),
                    Ok("true"),
                    "CI must configure the disposable test database"
                );
                eprintln!("SKIP admin_registry_repair_acceptance: TWO_TEST_DATABASE_URL not set");
                None
            }
        }
    }

    async fn migrator(chain: &str) -> Migrator {
        Migrator::new(super::root().join(format!("crates/{chain}/migrations")))
            .await
            .expect("load actual repository migration chain")
    }

    fn row() -> Value {
        json!({
            "token_id": TOKEN,
            "secret_hash": hex::encode(sha2::Sha256::digest(b"synthetic-repair-acceptance-hash-input")),
            "scopes": ["read", "moderate"],
            "guild_ids": [1],
            "holder_id": "fixture-client-one",
            "holder_kind": "ai_agent",
            "label": "synthetic fixture",
            "issued_by": "fixture-operator",
            "issued_at": ISSUED,
            "expires_at": NINETY_DAYS,
            "revoked_at": null,
            "last_used_at": null
        })
    }

    use sha2::Digest;

    async fn insert(pool: &PgPool, value: &Value) -> Result<(), sqlx::Error> {
        // Populate the ACTUAL table's record type. On the frozen base missing
        // holder columns are ignored by Postgres, so negative probes fail as
        // accepted invalid rows, not as undefined-column/type errors.
        sqlx::query("INSERT INTO admin_api_tokens SELECT (jsonb_populate_record(NULL::admin_api_tokens, $1::jsonb)).*")
            .bind(value)
            .execute(pool)
            .await
            .map(|_| ())
    }

    async fn snapshot(pool: &PgPool) -> Value {
        sqlx::query_scalar("SELECT to_jsonb(t) FROM admin_api_tokens t WHERE token_id = $1")
            .bind(TOKEN)
            .fetch_one(pool)
            .await
            .expect("read entire registry row")
    }

    async fn clear(pool: &PgPool) {
        sqlx::query("DELETE FROM admin_api_tokens WHERE token_id = $1")
            .bind(TOKEN)
            .execute(pool)
            .await
            .expect("clear only this fixture row");
    }

    async fn refused(pool: &PgPool, value: &Value, state: &str) -> bool {
        let result = insert(pool, value).await;
        // Collect all probes and tear down before asserting, even on RED.
        clear(pool).await;
        match result {
            Ok(()) => false,
            Err(err) => {
                err.as_database_error()
                    .and_then(|err| err.code())
                    .as_deref()
                    == Some(state)
            }
        }
    }

    async fn holder_and_boundary_controls(chain: &str) {
        let Some(url) = bootstrap() else { return };
        let db = TestDatabase::create(&url, &migrator(chain).await)
            .await
            .expect("create fully migrated isolated database");
        // Detect silently ignored holder keys, and require actual NOT NULL
        // columns. A nullable column plus a CHECK can otherwise accept NULL.
        let columns: Vec<(String, String)> = sqlx::query_as(
            "SELECT column_name, is_nullable FROM information_schema.columns
             WHERE table_schema = 'public' AND table_name = 'admin_api_tokens'
               AND column_name IN ('holder_id', 'holder_kind') ORDER BY column_name",
        )
        .fetch_all(db.pool())
        .await
        .expect("read authority columns");
        let mut controls = Vec::new();
        for (kind, scopes, expires, lifetime) in [
            ("human", json!(["read"]), NINETY_DAYS, 7_776_000.0),
            (
                "human",
                json!(["moderate", "admin"]),
                NINETY_DAYS,
                7_776_000.0,
            ),
            (
                "ai_agent",
                json!(["read", "moderate"]),
                NINETY_DAYS,
                7_776_000.0,
            ),
            ("human", json!(["admin"]), "2026-09-01T00:00:01Z", 1.0),
            (
                "ai_agent",
                json!(["moderate"]),
                "2026-11-29T23:59:59Z",
                7_775_999.0,
            ),
        ] {
            let mut value = row();
            value["holder_kind"] = json!(kind);
            value["scopes"] = scopes.clone();
            value["expires_at"] = json!(expires);
            insert(db.pool(), &value)
                .await
                .expect("valid named holder with positive lifetime up to exactly 90 days inserts");
            let actual = snapshot(db.pool()).await;
            controls.push((
                actual["holder_id"].clone(),
                actual["holder_kind"].clone(),
                actual["scopes"].clone(),
                kind,
                scopes,
            ));
            let seconds: f64 = sqlx::query_scalar("SELECT EXTRACT(EPOCH FROM (expires_at - issued_at))::double precision FROM admin_api_tokens WHERE token_id = $1")
                .bind(TOKEN).fetch_one(db.pool()).await.expect("read exact lifetime");
            assert_eq!(
                seconds, lifetime,
                "valid lifetime must round-trip without truncation"
            );
            clear(db.pool()).await;
        }
        db.close().await.expect("drop owned fixture database");
        assert_eq!(
            columns,
            vec![
                ("holder_id".into(), "NO".into()),
                ("holder_kind".into(), "NO".into())
            ],
            "{chain}: holder authority must exist and be NOT NULL"
        );
        for (id, actual_kind, actual_scopes, kind, scopes) in controls {
            assert_eq!(
                id,
                json!("fixture-client-one"),
                "named principal must round-trip"
            );
            assert_eq!(actual_kind, json!(kind));
            assert_eq!(actual_scopes, scopes);
        }
    }

    async fn invalid_holders(chain: &str) {
        let Some(url) = bootstrap() else { return };
        let db = TestDatabase::create(&url, &migrator(chain).await)
            .await
            .expect("create isolated holder database");
        let mut probes = Vec::new();
        for field in ["holder_id", "holder_kind"] {
            let mut value = row();
            value.as_object_mut().expect("fixture object").remove(field);
            probes.push((
                format!("missing {field}"),
                refused(db.pool(), &value, "23502").await,
            ));
            value[field] = Value::Null;
            probes.push((
                format!("NULL {field}"),
                refused(db.pool(), &value, "23502").await,
            ));
            for invalid in ["", " ", "\t\r\n"] {
                value[field] = json!(invalid);
                probes.push((
                    format!("{field}={invalid:?}"),
                    refused(db.pool(), &value, "23514").await,
                ));
            }
        }
        for kind in ["robot", "AI_AGENT", "Human", "ai-agent", " human", "human "] {
            let mut value = row();
            value["holder_kind"] = json!(kind);
            probes.push((
                format!("holder_kind={kind:?}"),
                refused(db.pool(), &value, "23514").await,
            ));
        }
        // Omitted SQL columns must not acquire a fabricated default holder.
        // jsonb_populate_record above expands missing keys to explicit NULL;
        // these probes separately exercise actual INSERT omission/defaults.
        for statement in [
            "INSERT INTO admin_api_tokens (token_id, secret_hash, scopes, guild_ids, label, issued_by, issued_at, expires_at) VALUES ($1, $2, '{read}', '{1}', 'fixture-label', 'fixture-issuer', $3::timestamptz, $4::timestamptz)",
            "INSERT INTO admin_api_tokens (token_id, secret_hash, scopes, guild_ids, label, issued_by, holder_id, issued_at, expires_at) VALUES ($1, $2, '{read}', '{1}', 'fixture-label', 'fixture-issuer', 'fixture-holder', $3::timestamptz, $4::timestamptz)",
            "INSERT INTO admin_api_tokens (token_id, secret_hash, scopes, guild_ids, label, issued_by, holder_kind, issued_at, expires_at) VALUES ($1, $2, '{read}', '{1}', 'fixture-label', 'fixture-issuer', 'human', $3::timestamptz, $4::timestamptz)",
        ] {
            let hash = row()["secret_hash"].as_str().expect("fixture hash").to_owned();
            let result = sqlx::query(statement).bind(TOKEN).bind(hash).bind(ISSUED).bind(NINETY_DAYS).execute(db.pool()).await;
            clear(db.pool()).await;
            let rejected = result.err().is_some_and(|err| err.as_database_error().and_then(|err| err.code()).as_deref() == Some("23502"));
            probes.push((format!("omitted authority: {statement}"), rejected));
        }
        db.close().await.expect("drop holder database");
        assert!(probes.iter().all(|(_, refused)| *refused), "{chain}: missing/blank/unknown holder authority must refuse with constraint SQLSTATE: {probes:?}");
    }

    async fn ai_admin_refused(chain: &str) {
        let Some(url) = bootstrap() else { return };
        let db = TestDatabase::create(&url, &migrator(chain).await)
            .await
            .expect("create isolated AI database");
        let mut probes = Vec::new();
        for scopes in [json!(["admin"]), json!(["read", "moderate", "admin"])] {
            let mut value = row();
            value["scopes"] = scopes.clone();
            probes.push((scopes, refused(db.pool(), &value, "23514").await));
        }
        db.close().await.expect("drop AI database");
        assert!(
            probes.iter().all(|(_, refused)| *refused),
            "{chain}: AI must never hold admin: {probes:?}"
        );
    }

    async fn lifetime_refusals(chain: &str) {
        let Some(url) = bootstrap() else { return };
        let db = TestDatabase::create(&url, &migrator(chain).await)
            .await
            .expect("create isolated lifetime database");
        let mut probes = Vec::new();
        for (issued, expires) in [
            (ISSUED, "2026-11-30T00:00:00.000001Z"),
            (ISSUED, "2026-11-30T00:00:01Z"),
            (ISSUED, "2027-09-01T00:00:00Z"),
            (ISSUED, ISSUED),
            (ISSUED, "2026-08-31T23:59:59Z"),
            (ISSUED, "infinity"),
            ("-infinity", NINETY_DAYS),
            ("-infinity", "infinity"),
            ("infinity", "infinity"),
        ] {
            let mut value = row();
            value["issued_at"] = json!(issued);
            value["expires_at"] = json!(expires);
            probes.push((
                format!("{issued} -> {expires}"),
                refused(db.pool(), &value, "23514").await,
            ));
        }
        for field in ["issued_at", "expires_at", "scopes", "guild_ids"] {
            let mut value = row();
            value.as_object_mut().expect("fixture object").remove(field);
            probes.push((
                format!("missing {field}"),
                refused(db.pool(), &value, "23502").await,
            ));
            value[field] = Value::Null;
            probes.push((
                format!("NULL {field}"),
                refused(db.pool(), &value, "23502").await,
            ));
        }
        db.close().await.expect("drop lifetime database");
        assert!(probes.iter().all(|(_, refused)| *refused), "{chain}: lifetime must be finite, positive and <=90 days; missing/NULL authority refuses: {probes:?}");
    }

    async fn updates_recheck_authority(chain: &str) {
        let Some(url) = bootstrap() else { return };
        let db = TestDatabase::create(&url, &migrator(chain).await)
            .await
            .expect("create isolated update database");
        let mut probes = Vec::new();
        for (statement, state) in [
            ("UPDATE admin_api_tokens SET scopes = '{admin}' WHERE token_id = $1", "23514"),
            ("UPDATE admin_api_tokens SET expires_at = issued_at + interval '90 days 1 second' WHERE token_id = $1", "23514"),
            ("UPDATE admin_api_tokens SET issued_at = '-infinity' WHERE token_id = $1", "23514"),
            ("UPDATE admin_api_tokens SET holder_kind = 'robot' WHERE token_id = $1", "23514"),
            ("UPDATE admin_api_tokens SET holder_kind = NULL WHERE token_id = $1", "23502"),
            ("UPDATE admin_api_tokens SET holder_id = NULL WHERE token_id = $1", "23502"),
            ("UPDATE admin_api_tokens SET holder_id = ' ' WHERE token_id = $1", "23514"),
        ] {
            insert(db.pool(), &row()).await.expect("valid AI moderate row inserts");
            let before = snapshot(db.pool()).await;
            let result = sqlx::query(statement).bind(TOKEN).execute(db.pool()).await;
            let after = snapshot(db.pool()).await;
            let rejected = result.err().is_some_and(|err| err.as_database_error().and_then(|err| err.code()).as_deref() == Some(state));
            probes.push((statement, rejected, after == before));
            clear(db.pool()).await;
        }
        let mut human = row();
        human["holder_kind"] = json!("human");
        human["scopes"] = json!(["admin"]);
        insert(db.pool(), &human)
            .await
            .expect("human explicit admin control inserts");
        let before = snapshot(db.pool()).await;
        let result =
            sqlx::query("UPDATE admin_api_tokens SET holder_kind = 'ai_agent' WHERE token_id = $1")
                .bind(TOKEN)
                .execute(db.pool())
                .await;
        let after = snapshot(db.pool()).await;
        let rejected = result.err().is_some_and(|err| {
            err.as_database_error()
                .and_then(|err| err.code())
                .as_deref()
                == Some("23514")
        });
        probes.push(("human admin changed to AI", rejected, after == before));
        db.close().await.expect("drop update database");
        assert!(probes.iter().all(|(_, rejected, unchanged)| *rejected && *unchanged), "{chain}: every invalid UPDATE must refuse with the expected constraint SQLSTATE and preserve the row: {probes:?}");
    }

    async fn catalog(pool: &PgPool) -> Value {
        sqlx::query_scalar(
            "SELECT jsonb_build_object(
             'columns', (SELECT jsonb_agg(to_jsonb(c) ORDER BY ordinal_position) FROM information_schema.columns c WHERE table_schema='public' AND table_name='admin_api_tokens'),
             'constraints', (SELECT jsonb_agg(jsonb_build_array(conname, pg_get_constraintdef(oid), convalidated) ORDER BY conname) FROM pg_constraint WHERE conrelid='admin_api_tokens'::regclass),
             'indexes', (SELECT jsonb_agg(indexdef ORDER BY indexname) FROM pg_indexes WHERE schemaname='public' AND tablename='admin_api_tokens'),
             'tables', (SELECT jsonb_agg(tablename ORDER BY tablename) FROM pg_tables WHERE schemaname='public'),
             'migrations', (SELECT jsonb_agg(to_jsonb(m) ORDER BY version) FROM _sqlx_migrations m))",
        ).fetch_one(pool).await.expect("snapshot schema and migration history")
    }

    async fn incompatible_upgrade_is_atomic(chain: &str, shipped: i64) {
        let Some(url) = bootstrap() else { return };
        let latest = migrator(chain).await;
        let prefix = Migrator::with_migrations(
            latest
                .iter()
                .filter(|m| m.version <= shipped)
                .cloned()
                .collect(),
        );
        let db = TestDatabase::create(&url, &prefix)
            .await
            .expect("create shipped-schema fixture");
        let mut legacy = row();
        legacy
            .as_object_mut()
            .expect("fixture object")
            .remove("holder_id");
        legacy
            .as_object_mut()
            .expect("fixture object")
            .remove("holder_kind");
        legacy["scopes"] = json!(["admin"]);
        legacy["expires_at"] = json!("2027-09-01T00:00:00Z");
        insert(db.pool(), &legacy)
            .await
            .expect("shipped schema accepts a legacy incompatible row");
        let before_row: Value = sqlx::query_scalar("SELECT coalesce(jsonb_agg(to_jsonb(t) ORDER BY token_id), '[]'::jsonb) FROM admin_api_tokens t")
            .fetch_one(db.pool()).await.expect("snapshot all legacy rows");
        let before_catalog = catalog(db.pool()).await;
        let result = latest.run(db.pool()).await;
        let after_row: Value = sqlx::query_scalar("SELECT coalesce(jsonb_agg(to_jsonb(t) ORDER BY token_id), '[]'::jsonb) FROM admin_api_tokens t")
            .fetch_one(db.pool()).await.expect("snapshot all rows after attempted upgrade");
        let after_catalog = catalog(db.pool()).await;
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM admin_api_tokens")
            .fetch_one(db.pool())
            .await
            .expect("count preserved rows");
        db.close().await.expect("drop failed-upgrade fixture");
        assert!(result.is_err(), "{chain}: forward migration must abort on incompatible legacy rows, never invent holders or truncate expiry");
        assert_eq!(count, 1, "never delete incompatible registry rows");
        assert_eq!(
            after_row, before_row,
            "no fabricated holder or silent lifetime truncation"
        );
        assert_eq!(after_catalog, before_catalog, "migration transaction must leave no partial DDL, unvalidated CHECK or applied-history entry");
    }

    // Separate functions preserve independent failures for the two real chains.
    #[tokio::test]
    async fn cutover_named_holders_and_exact_ninety_days() {
        holder_and_boundary_controls("cutover").await;
    }
    #[tokio::test]
    async fn store_named_holders_and_exact_ninety_days() {
        holder_and_boundary_controls("store").await;
    }
    #[tokio::test]
    async fn cutover_missing_blank_unknown_holders_refuse() {
        invalid_holders("cutover").await;
    }
    #[tokio::test]
    async fn store_missing_blank_unknown_holders_refuse() {
        invalid_holders("store").await;
    }
    #[tokio::test]
    async fn cutover_ai_admin_refuses() {
        ai_admin_refused("cutover").await;
    }
    #[tokio::test]
    async fn store_ai_admin_refuses() {
        ai_admin_refused("store").await;
    }
    #[tokio::test]
    async fn cutover_invalid_lifetime_and_null_authority_refuse() {
        lifetime_refusals("cutover").await;
    }
    #[tokio::test]
    async fn store_invalid_lifetime_and_null_authority_refuse() {
        lifetime_refusals("store").await;
    }
    #[tokio::test]
    async fn cutover_updates_cannot_bypass_constraints() {
        updates_recheck_authority("cutover").await;
    }
    #[tokio::test]
    async fn store_updates_cannot_bypass_constraints() {
        updates_recheck_authority("store").await;
    }
    #[tokio::test]
    async fn cutover_incompatible_upgrade_aborts_without_partial_ddl() {
        incompatible_upgrade_is_atomic("cutover", 456).await;
    }
    #[tokio::test]
    async fn store_incompatible_upgrade_aborts_without_partial_ddl() {
        incompatible_upgrade_is_atomic("store", 457).await;
    }
}
