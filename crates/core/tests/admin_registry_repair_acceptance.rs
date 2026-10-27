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
        let (prefix, latest) = pre_repair_migrators(chain, shipped).await;
        let db = TestDatabase::create(&url, &prefix)
            .await
            .expect("create pre-repair-schema fixture");
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

    async fn pre_repair_migrators(chain: &str, shipped: i64) -> (Migrator, Migrator) {
        // Apply unrelated predecessors too: a refusal must roll back the repair,
        // not accidentally compare history across a successful earlier migration.
        let first_repair = super::forward_registry_files(chain, shipped)
            .first()
            .expect("forward registry repair exists")
            .file_name()
            .expect("repair filename")
            .to_str()
            .expect("UTF-8 repair filename")
            .split('_')
            .next()
            .expect("repair version")
            .parse::<i64>()
            .expect("numeric repair version");
        let latest = migrator(chain).await;
        let prefix = Migrator::with_migrations(
            latest
                .iter()
                .filter(|m| m.version < first_repair)
                .cloned()
                .collect(),
        );
        (prefix, latest)
    }

    async fn registry_rows(pool: &PgPool) -> Value {
        sqlx::query_scalar("SELECT coalesce(jsonb_agg(to_jsonb(t) ORDER BY token_id), '[]'::jsonb) FROM admin_api_tokens t")
            .fetch_one(pool).await.expect("snapshot all partial-registry data")
    }

    async fn omitted_holder_inserts(pool: &PgPool) -> Vec<(&'static str, String)> {
        let mut probes = Vec::new();
        for (name, statement) in [
            ("both omitted", "INSERT INTO admin_api_tokens (token_id, secret_hash, scopes, guild_ids, label, issued_by, issued_at, expires_at) VALUES ($1, $2, '{read}', '{1}', 'fixture-label', 'fixture-issuer', $3::timestamptz, $4::timestamptz)"),
            ("holder_id omitted", "INSERT INTO admin_api_tokens (token_id, secret_hash, scopes, guild_ids, label, issued_by, holder_kind, issued_at, expires_at) VALUES ($1, $2, '{read}', '{1}', 'fixture-label', 'fixture-issuer', 'human', $3::timestamptz, $4::timestamptz)"),
            ("holder_kind omitted", "INSERT INTO admin_api_tokens (token_id, secret_hash, scopes, guild_ids, label, issued_by, holder_id, issued_at, expires_at) VALUES ($1, $2, '{read}', '{1}', 'fixture-label', 'fixture-issuer', 'fixture-holder', $3::timestamptz, $4::timestamptz)"),
        ] {
            let hash = row()["secret_hash"].as_str().expect("fixture hash").to_owned();
            let result = sqlx::query(statement)
                .bind(TOKEN)
                .bind(hash)
                .bind(ISSUED)
                .bind(NINETY_DAYS)
                .execute(pool)
                .await;
            let outcome = match result {
                Ok(_) => "ACCEPTED fallback holder".to_owned(),
                Err(error) => error
                    .as_database_error()
                    .and_then(|error| error.code())
                    .map(|code| code.into_owned())
                    .unwrap_or_else(|| "non-database failure".to_owned()),
            };
            clear(pool).await;
            probes.push((name, outcome));
        }
        probes
    }

    async fn partial_holder_defaults_refuse_atomically(chain: &str, shipped: i64) {
        let Some(url) = bootstrap() else { return };
        let (prefix, latest) = pre_repair_migrators(chain, shipped).await;
        let mut failures = Vec::new();
        // The defaults are valid principals on purpose. Row/NOT NULL checks
        // alone cannot detect the omission-based authority they would permit.
        for (name, has_id, has_kind, default_id, default_kind) in [
            ("id column only", true, false, true, false),
            ("kind column only", false, true, false, true),
            ("both columns, id default only", true, true, true, false),
            ("both columns, kind default only", true, true, false, true),
            ("both columns, both defaults", true, true, true, true),
        ] {
            for populated in [false, true] {
                let db = TestDatabase::create(&url, &prefix)
                    .await
                    .expect("create isolated pre-repair default fixture");
                for (column, present, has_default, literal) in [
                    (
                        "holder_id",
                        has_id,
                        default_id,
                        "'fixture-fallback-principal'",
                    ),
                    ("holder_kind", has_kind, default_kind, "'human'"),
                ] {
                    if present {
                        // Both nullable-empty and NOT NULL-populated catalogs
                        // must survive refusal byte-for-byte in the snapshot.
                        let nullability = if populated { "NOT NULL" } else { "" };
                        let default = if has_default {
                            format!("DEFAULT {literal}")
                        } else {
                            String::new()
                        };
                        sqlx::query(sqlx::AssertSqlSafe(format!("ALTER TABLE admin_api_tokens ADD COLUMN {column} text {default} {nullability}")))
                            .execute(db.pool()).await.expect("install partial holder column");
                    }
                }
                if populated {
                    let mut existing = row();
                    existing["holder_kind"] = json!("human");
                    insert(db.pool(), &existing)
                        .await
                        .expect("insert otherwise valid pre-repair named fixture");
                }
                let before_rows = registry_rows(db.pool()).await;
                let before_catalog = catalog(db.pool()).await;
                let result = latest.run(db.pool()).await;
                // Snapshot BEFORE behavior probes so even a permissive repair's
                // DDL/data/history changes cannot be obscured by probe cleanup.
                let after_rows = registry_rows(db.pool()).await;
                let after_catalog = catalog(db.pool()).await;
                let omissions = if result.is_ok() {
                    clear(db.pool()).await;
                    omitted_holder_inserts(db.pool()).await
                } else {
                    Vec::new()
                };
                db.close().await.expect("drop partial-default fixture");
                if result.is_ok() || after_rows != before_rows || after_catalog != before_catalog {
                    failures.push(format!(
                        "{name}, populated={populated}: refused={}, data_preserved={}, catalog_and_history_preserved={}, omissions={omissions:?}",
                        result.is_err(), after_rows == before_rows, after_catalog == before_catalog
                    ));
                }
            }
        }
        assert!(failures.is_empty(), "{chain}: reject any pre-existing holder default atomically; do not DROP DEFAULT, fabricate authority, alter nullability/constraints/history or leave added DDL: {failures:#?}");
    }

    async fn partial_holders_without_defaults_upgrade(chain: &str, shipped: i64) {
        let Some(url) = bootstrap() else { return };
        let (prefix, latest) = pre_repair_migrators(chain, shipped).await;
        let mut failures = Vec::new();
        for (name, has_id, has_kind, populated) in [
            ("empty id column only", true, false, false),
            ("empty kind column only", false, true, false),
            ("empty both columns", true, true, false),
            ("populated named columns", true, true, true),
        ] {
            let db = TestDatabase::create(&url, &prefix)
                .await
                .expect("create isolated no-default control");
            for (column, present) in [("holder_id", has_id), ("holder_kind", has_kind)] {
                if present {
                    sqlx::query(sqlx::AssertSqlSafe(format!(
                        "ALTER TABLE admin_api_tokens ADD COLUMN {column} text"
                    )))
                    .execute(db.pool())
                    .await
                    .expect("install no-default partial column");
                }
            }
            if populated {
                let mut existing = row();
                existing["holder_kind"] = json!("human");
                insert(db.pool(), &existing)
                    .await
                    .expect("valid named pre-repair row inserts");
            }
            let before_rows = registry_rows(db.pool()).await;
            let result = latest.run(db.pool()).await;
            let after_rows = registry_rows(db.pool()).await;
            let columns: Vec<(String, String, Option<String>)> = sqlx::query_as(
                "SELECT column_name, is_nullable, column_default FROM information_schema.columns
                 WHERE table_schema='public' AND table_name='admin_api_tokens'
                   AND column_name IN ('holder_id', 'holder_kind') ORDER BY column_name",
            )
            .fetch_all(db.pool())
            .await
            .expect("read migrated no-default authority columns");
            let omissions = if result.is_ok() {
                clear(db.pool()).await;
                omitted_holder_inserts(db.pool()).await
            } else {
                Vec::new()
            };
            db.close().await.expect("drop no-default control");
            if result.is_err()
                || after_rows != before_rows
                || columns
                    != vec![
                        ("holder_id".into(), "NO".into(), None),
                        ("holder_kind".into(), "NO".into(), None),
                    ]
                || omissions.len() != 3
                || omissions.iter().any(|(_, state)| state != "23502")
            {
                failures.push(format!("{name}: migrated={}, data_preserved={}, columns={columns:?}, omissions={omissions:?}", result.is_ok(), after_rows == before_rows));
            }
        }
        assert!(failures.is_empty(), "{chain}: compatible partial schemas without defaults must migrate, preserve named data and refuse every omitted-holder INSERT with NOT NULL SQLSTATE 23502: {failures:#?}");
    }

    async fn holder_type_catalog(pool: &PgPool) -> Value {
        // information_schema.column_default does not expose domain fallbacks.
        // Include raw type identities/defaults and domain CHECKs, as well as the
        // ordinary catalog, so refusal cannot silently coerce or rewrite them.
        sqlx::query_scalar(
            "SELECT jsonb_build_object(
             'registry', $1::jsonb,
             'attributes', (SELECT jsonb_agg(jsonb_build_object(
                 'name', a.attname, 'type_oid', a.atttypid, 'type', format_type(a.atttypid, a.atttypmod),
                 'typmod', a.atttypmod, 'not_null', a.attnotnull, 'has_default', a.atthasdef,
                 'identity', a.attidentity, 'generated', a.attgenerated, 'collation', a.attcollation,
                 'default', pg_get_expr(d.adbin, d.adrelid)) ORDER BY a.attnum)
                 FROM pg_attribute a LEFT JOIN pg_attrdef d ON d.adrelid=a.attrelid AND d.adnum=a.attnum
                 WHERE a.attrelid='admin_api_tokens'::regclass AND a.attnum>0 AND NOT a.attisdropped),
             'domains', (SELECT jsonb_agg(to_jsonb(t) ORDER BY t.typname) FROM pg_type t
                 WHERE t.typnamespace='public'::regnamespace AND t.typtype='d'),
             'domain_constraints', (SELECT jsonb_agg(jsonb_build_object(
                 'catalog', to_jsonb(c), 'definition', pg_get_constraintdef(c.oid)) ORDER BY c.contypid, c.conname)
                 FROM pg_constraint c JOIN pg_type t ON t.oid=c.contypid
                 WHERE t.typnamespace='public'::regnamespace AND t.typtype='d'))",
        )
        .bind(catalog(pool).await)
        .fetch_one(pool)
        .await
        .expect("snapshot holder types, domain definitions and migration history")
    }

    async fn admin_omission_probes(pool: &PgPool) -> Vec<(&'static str, String)> {
        let mut probes = Vec::new();
        clear(pool).await;
        // Real omitted-column INSERTs, not jsonb_populate_record (which supplies
        // NULL instead of invoking a default). Every probe requests admin scope.
        for (name, statement) in [
            ("holder_kind omitted", "INSERT INTO admin_api_tokens (token_id, secret_hash, scopes, guild_ids, label, issued_by, holder_id, issued_at, expires_at) VALUES ($1, $2, '{admin}', '{1}', 'domain-probe', 'fixture-issuer', 'fixture-explicit-principal', $3::timestamptz, $4::timestamptz) RETURNING to_jsonb(admin_api_tokens)"),
            ("holder_id omitted", "INSERT INTO admin_api_tokens (token_id, secret_hash, scopes, guild_ids, label, issued_by, holder_kind, issued_at, expires_at) VALUES ($1, $2, '{admin}', '{1}', 'domain-probe', 'fixture-issuer', 'human', $3::timestamptz, $4::timestamptz) RETURNING to_jsonb(admin_api_tokens)"),
            ("both omitted", "INSERT INTO admin_api_tokens (token_id, secret_hash, scopes, guild_ids, label, issued_by, issued_at, expires_at) VALUES ($1, $2, '{admin}', '{1}', 'domain-probe', 'fixture-issuer', $3::timestamptz, $4::timestamptz) RETURNING to_jsonb(admin_api_tokens)"),
        ] {
            let hash = row()["secret_hash"].as_str().expect("fixture hash").to_owned();
            let result: Result<Value, _> = sqlx::query_scalar(statement)
                .bind(TOKEN)
                .bind(hash)
                .bind(ISSUED)
                .bind(NINETY_DAYS)
                .fetch_one(pool)
                .await;
            let outcome = match result {
                Ok(actual) => format!(
                    "ACCEPTED fabricated admin: holder_id={}, holder_kind={}, scopes={}",
                    actual["holder_id"], actual["holder_kind"], actual["scopes"]
                ),
                Err(error) => error
                    .as_database_error()
                    .and_then(|error| error.code())
                    .map(|code| code.into_owned())
                    .unwrap_or_else(|| "non-database failure".to_owned()),
            };
            // Prevent duplicate-token failures from masquerading as refusal.
            clear(pool).await;
            probes.push((name, outcome));
        }
        probes
    }

    async fn domain_holder_defaults_refuse_atomically(chain: &str, shipped: i64) {
        let Some(url) = bootstrap() else { return };
        let (prefix, latest) = pre_repair_migrators(chain, shipped).await;
        let mut failures = Vec::new();
        for (name, has_id, has_kind, domain_id, domain_kind) in [
            ("id domain, kind absent", true, false, true, false),
            ("kind domain, id absent", false, true, false, true),
            ("id domain, plain kind", true, true, true, false),
            ("plain id, kind domain", true, true, false, true),
            ("both domains", true, true, true, true),
        ] {
            for populated in [false, true] {
                let db = TestDatabase::create(&url, &prefix)
                    .await
                    .expect("create pre-repair domain-default fixture");
                for (column, present, domain, literal, predicate) in [
                    (
                        "holder_id",
                        has_id,
                        domain_id,
                        "'fixture-domain-principal'",
                        "VALUE <> ''",
                    ),
                    (
                        "holder_kind",
                        has_kind,
                        domain_kind,
                        "'human'",
                        "VALUE IN ('human', 'ai_agent')",
                    ),
                ] {
                    if !present {
                        continue;
                    }
                    let type_name = if domain {
                        let type_name = format!("fixture_{column}_domain");
                        sqlx::query(sqlx::AssertSqlSafe(format!(
                            "CREATE DOMAIN {type_name} AS pg_catalog.text DEFAULT {literal} CONSTRAINT {type_name}_valid CHECK ({predicate})"
                        )))
                        .execute(db.pool()).await.expect("create valid default-bearing text domain");
                        type_name
                    } else {
                        "pg_catalog.text".to_owned()
                    };
                    let nullability = if populated { "NOT NULL" } else { "" };
                    sqlx::query(sqlx::AssertSqlSafe(format!(
                        "ALTER TABLE admin_api_tokens ADD COLUMN {column} {type_name} {nullability}"
                    )))
                    .execute(db.pool())
                    .await
                    .expect("install holder without a column default");
                }
                let column_defaults: i64 = sqlx::query_scalar(
                    "SELECT count(*) FROM pg_attribute a WHERE a.attrelid='admin_api_tokens'::regclass
                     AND a.attname IN ('holder_id', 'holder_kind') AND (a.atthasdef OR EXISTS (
                         SELECT 1 FROM pg_attrdef d WHERE d.adrelid=a.attrelid AND d.adnum=a.attnum))",
                ).fetch_one(db.pool()).await.expect("inspect raw column-default metadata");
                let domain_defaults: i64 = sqlx::query_scalar(
                    "SELECT count(*) FROM pg_attribute a JOIN pg_type t ON t.oid=a.atttypid
                     WHERE a.attrelid='admin_api_tokens'::regclass AND a.attname IN ('holder_id', 'holder_kind')
                     AND t.typtype='d' AND t.typbasetype='pg_catalog.text'::regtype
                     AND t.typdefault IS NOT NULL AND t.typdefaultbin IS NOT NULL",
                ).fetch_one(db.pool()).await.expect("inspect independent domain-default metadata");
                assert_eq!(
                    column_defaults, 0,
                    "fixture must bypass an atthasdef-only guard"
                );
                assert_eq!(
                    domain_defaults,
                    i64::from(domain_id) + i64::from(domain_kind)
                );
                if populated {
                    let mut human = row();
                    human["holder_kind"] = json!("human");
                    human["scopes"] = json!(["admin"]);
                    insert(db.pool(), &human)
                        .await
                        .expect("insert otherwise valid named human admin fixture");
                }
                let before_rows = registry_rows(db.pool()).await;
                let before_catalog = holder_type_catalog(db.pool()).await;
                let result = latest.run(db.pool()).await;
                // All preservation evidence precedes cleanup and bypass probes.
                let after_rows = registry_rows(db.pool()).await;
                let after_catalog = holder_type_catalog(db.pool()).await;
                let omissions = if result.is_ok() {
                    admin_omission_probes(db.pool()).await
                } else {
                    Vec::new()
                };
                db.close().await.expect("drop owned domain-default fixture");
                if result.is_ok() || after_rows != before_rows || after_catalog != before_catalog {
                    failures.push(format!(
                        "{name}, populated={populated}: refused={}, rows_preserved={}, types_domains_catalog_history_preserved={}, admin_omissions={omissions:?}",
                        result.is_err(), after_rows == before_rows, after_catalog == before_catalog
                    ));
                }
            }
        }
        assert!(failures.is_empty(), "{chain}: domain fallbacks must refuse before authority DDL; never fabricate holders, coerce types, drop defaults or rewrite domains/constraints/history: {failures:#?}");
    }

    async fn noncanonical_holder_types_refuse_atomically(chain: &str, shipped: i64) {
        let Some(url) = bootstrap() else { return };
        let (prefix, latest) = pre_repair_migrators(chain, shipped).await;
        let mut failures = Vec::new();
        // The supported boundary is exact pg_catalog.TEXT, not arbitrary domain
        // or varchar support/conversion. No-default domains must refuse too.
        for column in ["holder_id", "holder_kind"] {
            for type_name in ["fixture_no_default_domain", "pg_catalog.varchar(80)"] {
                for populated in [false, true] {
                    let db = TestDatabase::create(&url, &prefix)
                        .await
                        .expect("create noncanonical holder-type fixture");
                    sqlx::query("CREATE DOMAIN fixture_no_default_domain AS pg_catalog.text CONSTRAINT fixture_nonempty CHECK (VALUE <> '')")
                        .execute(db.pool()).await.expect("create domain without any default");
                    for field in ["holder_id", "holder_kind"] {
                        let selected = if column == field {
                            type_name
                        } else {
                            "pg_catalog.text"
                        };
                        sqlx::query(sqlx::AssertSqlSafe(format!(
                            "ALTER TABLE admin_api_tokens ADD COLUMN {field} {selected}"
                        )))
                        .execute(db.pool())
                        .await
                        .expect("install noncanonical holder and plain peer");
                    }
                    let defaults: i64 = sqlx::query_scalar(
                        "SELECT count(*) FROM pg_attribute a JOIN pg_type t ON t.oid=a.atttypid
                         WHERE a.attrelid='admin_api_tokens'::regclass AND a.attname IN ('holder_id', 'holder_kind')
                         AND (a.atthasdef OR t.typdefault IS NOT NULL OR t.typdefaultbin IS NOT NULL)",
                    ).fetch_one(db.pool()).await.expect("verify no column or type defaults");
                    assert_eq!(
                        defaults, 0,
                        "no-default type fixture is independent of the domain fallback"
                    );
                    if populated {
                        insert(db.pool(), &row())
                            .await
                            .expect("insert named AI non-admin control");
                    }
                    let before_rows = registry_rows(db.pool()).await;
                    let before_catalog = holder_type_catalog(db.pool()).await;
                    let result = latest.run(db.pool()).await;
                    let after_rows = registry_rows(db.pool()).await;
                    let after_catalog = holder_type_catalog(db.pool()).await;
                    db.close().await.expect("drop noncanonical holder fixture");
                    if result.is_ok()
                        || before_rows != after_rows
                        || before_catalog != after_catalog
                    {
                        failures.push(format!("{column} {type_name}, populated={populated}: refused={}, rows_preserved={}, catalog_preserved={}", result.is_err(), before_rows == after_rows, before_catalog == after_catalog));
                    }
                }
            }
        }
        assert!(failures.is_empty(), "{chain}: noncanonical no-default holder types must refuse atomically, not be coerced or silently supported: {failures:#?}");
    }

    async fn plain_text_holder_type_controls(chain: &str, shipped: i64) {
        let Some(url) = bootstrap() else { return };
        let (prefix, latest) = pre_repair_migrators(chain, shipped).await;
        let mut failures = Vec::new();
        for (has_id, has_kind) in [(false, false), (true, false), (false, true), (true, true)] {
            // Populated partials with an absent holder cannot satisfy NOT NULL;
            // those refusals are covered elsewhere. Both named types do survive.
            for populated in [false, true] {
                if populated && (!has_id || !has_kind) {
                    continue;
                }
                let db = TestDatabase::create(&url, &prefix)
                    .await
                    .expect("create exact-TEXT positive control");
                for (column, present) in [("holder_id", has_id), ("holder_kind", has_kind)] {
                    if present {
                        sqlx::query(sqlx::AssertSqlSafe(format!(
                            "ALTER TABLE admin_api_tokens ADD COLUMN {column} pg_catalog.text"
                        )))
                        .execute(db.pool())
                        .await
                        .expect("install plain no-default holder");
                    }
                }
                if populated {
                    for (token, kind, scopes) in [
                        (TOKEN, "human", json!(["admin"])),
                        (
                            "adm_fedcba9876543210",
                            "ai_agent",
                            json!(["read", "moderate"]),
                        ),
                    ] {
                        let mut valid = row();
                        valid["token_id"] = json!(token);
                        valid["secret_hash"] =
                            json!(hex::encode(sha2::Sha256::digest(token.as_bytes())));
                        valid["holder_kind"] = json!(kind);
                        valid["scopes"] = scopes;
                        insert(db.pool(), &valid)
                            .await
                            .expect("insert explicit named positive control");
                    }
                }
                let before_rows = registry_rows(db.pool()).await;
                let result = latest.run(db.pool()).await;
                let after_rows = registry_rows(db.pool()).await;
                let canonical: i64 = sqlx::query_scalar(
                    "SELECT count(*) FROM pg_attribute a WHERE a.attrelid='admin_api_tokens'::regclass
                     AND a.attname IN ('holder_id', 'holder_kind') AND a.atttypid='pg_catalog.text'::regtype
                     AND a.atttypmod=-1 AND a.attnotnull AND NOT a.atthasdef AND NOT EXISTS (
                         SELECT 1 FROM pg_attrdef d WHERE d.adrelid=a.attrelid AND d.adnum=a.attnum)",
                ).fetch_one(db.pool()).await.expect("verify exact TEXT without defaults after repair");
                let omissions = if result.is_ok() {
                    admin_omission_probes(db.pool()).await
                } else {
                    Vec::new()
                };
                let writes = if result.is_ok() {
                    authority_write_probes(db.pool()).await
                } else {
                    Vec::new()
                };
                db.close().await.expect("drop plain TEXT positive control");
                if result.is_err()
                    || before_rows != after_rows
                    || canonical != 2
                    || omissions.len() != 3
                    || omissions.iter().any(|(_, state)| state != "23502")
                    || !writes.is_empty()
                {
                    failures.push(format!("id={has_id}, kind={has_kind}, populated={populated}: migrated={}, rows_preserved={}, canonical={canonical}, admin_omissions={omissions:?}, writes={writes:?}", result.is_ok(), before_rows == after_rows));
                }
            }
        }
        assert!(failures.is_empty(), "{chain}: absent or plain TEXT no-default holders must migrate, preserve explicit human-admin/AI-nonadmin rows and refuse omitted authority with NOT NULL 23502: {failures:#?}");
    }

    #[tokio::test]
    async fn cutover_domain_holder_defaults_refuse_atomically() {
        domain_holder_defaults_refuse_atomically("cutover", 456).await;
    }
    #[tokio::test]
    async fn store_domain_holder_defaults_refuse_atomically() {
        domain_holder_defaults_refuse_atomically("store", 457).await;
    }
    #[tokio::test]
    async fn cutover_noncanonical_holder_types_refuse_atomically() {
        noncanonical_holder_types_refuse_atomically("cutover", 456).await;
    }
    #[tokio::test]
    async fn store_noncanonical_holder_types_refuse_atomically() {
        noncanonical_holder_types_refuse_atomically("store", 457).await;
    }
    #[tokio::test]
    async fn cutover_plain_text_holder_type_controls() {
        plain_text_holder_type_controls("cutover", 456).await;
    }
    #[tokio::test]
    async fn store_plain_text_holder_type_controls() {
        plain_text_holder_type_controls("store", 457).await;
    }

    async fn holder_collation_catalog(pool: &PgPool) -> Value {
        sqlx::query_scalar(
            "SELECT jsonb_build_object(
             'holder_types_and_registry', $1::jsonb,
             'collations', (SELECT jsonb_agg(jsonb_build_object(
                 'namespace', n.nspname, 'catalog', to_jsonb(c)) ORDER BY c.oid)
                 FROM pg_collation c JOIN pg_namespace n ON n.oid=c.collnamespace
                 WHERE c.collnamespace='public'::regnamespace OR c.oid IN (
                     SELECT a.attcollation FROM pg_attribute a
                     WHERE a.attrelid='admin_api_tokens'::regclass
                       AND a.attnum>0 AND NOT a.attisdropped)))",
        )
        .bind(holder_type_catalog(pool).await)
        .fetch_one(pool)
        .await
        .expect("snapshot collation definitions, holder attributes, rows' schema and history")
    }

    async fn install_case_insensitive_collation(pool: &PgPool) -> Result<(), &'static str> {
        // This is an owned disposable database, never a live registry. Missing
        // ICU/CREATE COLLATION permission is fixture infrastructure failure,
        // not evidence that the migration rejected incompatible authority.
        sqlx::query("CREATE COLLATION fixture_holder_ci (provider=icu, locale='und-u-ks-level2', deterministic=false)")
            .execute(pool).await.map_err(|_| "INFRA: disposable PostgreSQL must support ICU collation fixtures; do not add grants or change the host")?;
        let semantics: (bool, bool, bool, bool) = sqlx::query_as(
            "SELECT c.collprovider='i', NOT c.collisdeterministic,
             'HUMAN'::text COLLATE fixture_holder_ci = 'human'::text COLLATE fixture_holder_ci,
             convert_to('HUMAN', 'UTF8') <> convert_to('human', 'UTF8')
             FROM pg_collation c WHERE c.oid='fixture_holder_ci'::regcollation",
        )
        .fetch_one(pool)
        .await
        .map_err(|_| "INFRA: cannot verify actual ICU metadata and case-insensitive equality")?;
        if semantics != (true, true, true, true) {
            return Err("INFRA: fixture requires genuinely nondeterministic equality across different bytes");
        }
        Ok(())
    }

    async fn collation_insert_probe(
        pool: &PgPool,
        kind: &str,
        scopes: &[&str],
    ) -> Result<Value, (String, Option<String>)> {
        // A distinct, synthetic token and hash avoid duplicate-key interference.
        // Roll back each probe, leaving preservation/control rows untouched.
        let token = "adm_c011a71000000001";
        let hash = hex::encode(sha2::Sha256::digest(b"synthetic-collation-probe-only"));
        let mut tx = pool.begin().await.expect("begin isolated collation probe");
        let result = sqlx::query_scalar(
            "INSERT INTO admin_api_tokens
             (token_id, secret_hash, scopes, guild_ids, holder_id, holder_kind, label, issued_by, issued_at, expires_at)
             VALUES ($1, $2, $3, '{1}', 'fixture-collation-principal', $4, 'collation-fixture', 'fixture-operator', $5::timestamptz, $6::timestamptz)
             RETURNING to_jsonb(admin_api_tokens)",
        )
        .bind(token)
        .bind(hash)
        .bind(scopes)
        .bind(kind)
        .bind(ISSUED)
        .bind(NINETY_DAYS)
        .fetch_one(&mut *tx)
        .await;
        tx.rollback()
            .await
            .expect("roll back owned collation probe");
        result.map_err(|error| {
            let database = error.as_database_error();
            (
                database
                    .and_then(|e| e.code())
                    .map(|c| c.into_owned())
                    .unwrap_or_else(|| "non-database failure".to_owned()),
                database.and_then(|e| e.constraint()).map(str::to_owned),
            )
        })
    }

    async fn nondeterministic_holder_collations_refuse_atomically(chain: &str, shipped: i64) {
        let Some(url) = bootstrap() else { return };
        let (prefix, latest) = pre_repair_migrators(chain, shipped).await;
        let mut failures = Vec::new();
        for (name, has_id, ci_id, ci_kind) in [
            ("kind only, id absent", false, false, true),
            ("canonical id, case-insensitive kind", true, false, true),
            ("case-insensitive id, canonical kind", true, true, false),
            ("both case-insensitive", true, true, true),
        ] {
            for populated in [false, true] {
                let db = TestDatabase::create(&url, &prefix)
                    .await
                    .expect("create pre-repair collation fixture");
                if let Err(limitation) = install_case_insensitive_collation(db.pool()).await {
                    db.close()
                        .await
                        .expect("drop fixture after ICU infrastructure failure");
                    panic!("{limitation}");
                }
                for (column, present, incompatible) in
                    [("holder_id", has_id, ci_id), ("holder_kind", true, ci_kind)]
                {
                    if present {
                        let collation = if incompatible {
                            "fixture_holder_ci"
                        } else {
                            "pg_catalog.\"default\""
                        };
                        let nullability = if populated { "NOT NULL" } else { "" };
                        sqlx::query(sqlx::AssertSqlSafe(format!(
                            "ALTER TABLE admin_api_tokens ADD COLUMN {column} pg_catalog.text COLLATE {collation} {nullability}"
                        ))).execute(db.pool()).await.expect("install exact TEXT with explicit fixture collation");
                    }
                }
                let identities: Vec<(String, bool, bool, bool)> = sqlx::query_as(
                    "SELECT a.attname::text, a.atttypid='pg_catalog.text'::regtype AND a.atttypmod=-1,
                     a.attcollation='fixture_holder_ci'::regcollation,
                     a.attcollation='pg_catalog.\"default\"'::regcollation
                     FROM pg_attribute a WHERE a.attrelid='admin_api_tokens'::regclass
                       AND a.attname IN ('holder_id', 'holder_kind') ORDER BY a.attname",
                ).fetch_all(db.pool()).await.expect("verify exact TEXT and actual column collation OIDs");
                let mut expected = Vec::new();
                if has_id {
                    expected.push(("holder_id".to_owned(), true, ci_id, !ci_id));
                }
                expected.push(("holder_kind".to_owned(), true, ci_kind, !ci_kind));
                assert_eq!(
                    identities, expected,
                    "fixture must isolate collation from type/domain/default mismatches"
                );
                if populated {
                    let mut existing = row();
                    existing["holder_kind"] = json!("human");
                    existing["scopes"] = json!(["admin"]);
                    insert(db.pool(), &existing)
                        .await
                        .expect("insert valid available holder fields before repair");
                }
                let before_rows = registry_rows(db.pool()).await;
                let before_catalog = holder_collation_catalog(db.pool()).await;
                let result = latest.run(db.pool()).await;
                // Capture everything before a diagnostic INSERT or teardown.
                let after_rows = registry_rows(db.pool()).await;
                let after_catalog = holder_collation_catalog(db.pool()).await;
                let (intended_refusal, migration_state) = match &result {
                    Err(sqlx::migrate::MigrateError::ExecuteMigration(error, _)) => {
                        let database = error.as_database_error();
                        let code = database
                            .and_then(|e| e.code())
                            .map(|c| c.into_owned())
                            .unwrap_or_default();
                        let meaningful = database.is_some_and(|e| {
                            let message = e.message().to_ascii_lowercase();
                            message.contains("collation") || message.contains("holder")
                        });
                        (
                            matches!(code.as_str(), "P0001" | "23514") && meaningful,
                            code,
                        )
                    }
                    Err(_) => (false, "migration infrastructure/history failure".to_owned()),
                    Ok(()) => (false, "ACCEPTED incompatible collation".to_owned()),
                };
                // The essential witness keeps holder_id canonical: PG17 cannot
                // hide kind equality behind nondeterministic-regex SQLSTATE 0A000.
                let diagnostic = if result.is_ok() {
                    match collation_insert_probe(db.pool(), "HUMAN", &["admin"]).await {
                        Ok(actual) => {
                            let preserved = actual["holder_id"] == json!("fixture-collation-principal")
                                && actual["holder_kind"] == json!("HUMAN")
                                && actual["scopes"] == json!(["admin"]);
                            format!(
                                "{}: holder_id={}, holder_kind={}, scopes={}",
                                if preserved { "ACCEPTED uppercase admin" } else { "INSERT rewrote authority; not the unchanged-uppercase witness" },
                                actual["holder_id"], actual["holder_kind"], actual["scopes"]
                            )
                        },
                        Err((state, constraint)) => format!("INSERT SQLSTATE={state}, constraint={constraint:?}; not an accepted bypass"),
                    }
                } else {
                    "not run: migration did not succeed".to_owned()
                };
                db.close().await.expect("drop owned collation fixture");
                if !intended_refusal || before_rows != after_rows || before_catalog != after_catalog
                {
                    failures.push(format!(
                        "{name}, populated={populated}: intended_refusal={intended_refusal}, migration={migration_state}, rows_preserved={}, attributes_collations_constraints_indexes_history_preserved={}, diagnostic={diagnostic}",
                        before_rows == after_rows, before_catalog == after_catalog
                    ));
                }
            }
        }
        assert!(failures.is_empty(), "{chain}: nondeterministic holder collation must refuse atomically without rewriting collations/types/defaults/nullability, inventing holders, changing rows or recording repair history; unrelated SQLSTATEs are not intended refusals: {failures:#?}");
    }

    async fn canonical_holder_collation_controls(chain: &str, shipped: i64) {
        let Some(url) = bootstrap() else { return };
        let (prefix, latest) = pre_repair_migrators(chain, shipped).await;
        let mut failures = Vec::new();
        // Absent holders can upgrade only on an empty registry. Populated rows
        // must already identify both holders; missing-authority refusal is kept.
        for (name, collation, populated) in [
            ("absent holders", None, false),
            ("default TEXT empty", Some("pg_catalog.\"default\""), false),
            (
                "default TEXT populated",
                Some("pg_catalog.\"default\""),
                true,
            ),
            ("deterministic C empty", Some("pg_catalog.\"C\""), false),
            ("deterministic C populated", Some("pg_catalog.\"C\""), true),
        ] {
            let db = TestDatabase::create(&url, &prefix)
                .await
                .expect("create canonical collation control");
            if let Some(collation) = collation {
                sqlx::query(sqlx::AssertSqlSafe(format!(
                    "ALTER TABLE admin_api_tokens ADD COLUMN holder_id pg_catalog.text COLLATE {collation}, ADD COLUMN holder_kind pg_catalog.text COLLATE {collation}"
                ))).execute(db.pool()).await.expect("install canonical deterministic holders");
            }
            if populated {
                for (token, kind, scopes) in [
                    (TOKEN, "human", json!(["admin"])),
                    (
                        "adm_fedcba9876543210",
                        "ai_agent",
                        json!(["read", "moderate"]),
                    ),
                ] {
                    let mut existing = row();
                    existing["token_id"] = json!(token);
                    existing["secret_hash"] =
                        json!(hex::encode(sha2::Sha256::digest(token.as_bytes())));
                    existing["holder_kind"] = json!(kind);
                    existing["scopes"] = scopes;
                    insert(db.pool(), &existing)
                        .await
                        .expect("insert explicit human admin and AI non-admin controls");
                }
            }
            let before_rows = registry_rows(db.pool()).await;
            let result = latest.run(db.pool()).await;
            let after_rows = registry_rows(db.pool()).await;
            let deterministic: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pg_attribute a JOIN pg_collation c ON c.oid=a.attcollation
                 WHERE a.attrelid='admin_api_tokens'::regclass AND a.attname IN ('holder_id', 'holder_kind')
                   AND a.atttypid='pg_catalog.text'::regtype AND a.atttypmod=-1 AND c.collisdeterministic",
            ).fetch_one(db.pool()).await.expect("verify upgraded holders use deterministic TEXT");
            let mut probes = Vec::new();
            if result.is_ok() {
                for (kind, scopes, valid) in [
                    ("human", vec!["admin"], true),
                    ("ai_agent", vec!["read", "moderate"], true),
                    ("HUMAN", vec!["admin"], false),
                    ("Human", vec!["admin"], false),
                    ("AI_AGENT", vec!["read"], false),
                    ("robot", vec!["read"], false),
                ] {
                    let probe = collation_insert_probe(db.pool(), kind, &scopes).await;
                    let correct = if valid {
                        probe.as_ref().is_ok_and(|actual| {
                            actual["holder_id"] == json!("fixture-collation-principal")
                                && actual["holder_kind"] == json!(kind)
                                && actual["scopes"] == json!(scopes)
                        })
                    } else {
                        matches!(&probe, Err((code, Some(constraint))) if code == "23514"
                            && matches!(constraint.as_str(), "admin_api_tokens_holder_kind_closed" | "admin_api_tokens_admin_human"))
                    };
                    if !correct {
                        let outcome = match probe {
                            Ok(_) => "ACCEPTED".to_owned(),
                            Err((state, constraint)) => {
                                format!("SQLSTATE={state}, constraint={constraint:?}")
                            }
                        };
                        probes.push(format!("kind={kind}, valid={valid}: {outcome}"));
                    }
                }
            }
            let after_probes = registry_rows(db.pool()).await;
            db.close().await.expect("drop canonical collation control");
            if result.is_err()
                || before_rows != after_rows
                || after_rows != after_probes
                || deterministic != 2
                || !probes.is_empty()
            {
                failures.push(format!("{name}: migrated={}, rows_preserved={}, probes_preserve_rows={}, deterministic={deterministic}, probes={probes:?}", result.is_ok(), before_rows == after_rows, after_rows == after_probes));
            }
        }
        assert!(failures.is_empty(), "{chain}: absent/default/canonical deterministic TEXT must upgrade, preserve named rows, accept lowercase authority and reject noncanonical kinds with real authority CHECK errors: {failures:#?}");
    }

    #[tokio::test]
    async fn cutover_nondeterministic_holder_collations_refuse_atomically() {
        nondeterministic_holder_collations_refuse_atomically("cutover", 456).await;
    }
    #[tokio::test]
    async fn store_nondeterministic_holder_collations_refuse_atomically() {
        nondeterministic_holder_collations_refuse_atomically("store", 457).await;
    }
    #[tokio::test]
    async fn cutover_canonical_holder_collation_controls() {
        canonical_holder_collation_controls("cutover", 456).await;
    }
    #[tokio::test]
    async fn store_canonical_holder_collation_controls() {
        canonical_holder_collation_controls("store", 457).await;
    }

    const AUTHORITY_CHECKS: [(&str, &str); 4] = [
        ("admin_api_tokens_holder_named", "holder_id <> ''"),
        ("admin_api_tokens_holder_kind_closed", "holder_kind <> ''"),
        ("admin_api_tokens_admin_human", "cardinality(scopes) > 0"),
        (
            "admin_api_tokens_lifetime_bounded",
            "expires_at > issued_at",
        ),
    ];

    async fn authority_checks(pool: &PgPool) -> Vec<(String, String)> {
        // Capture the exact deployed spelling ONLY for the compatibility
        // fixture. Independent write probes below decide whether it enforces
        // the approved contract; its own SQL is never an enforcement oracle.
        sqlx::query_as(
            "SELECT conname::text, pg_get_expr(conbin, conrelid)
             FROM pg_constraint WHERE conrelid='admin_api_tokens'::regclass
               AND conname IN ('admin_api_tokens_holder_named',
                   'admin_api_tokens_holder_kind_closed',
                   'admin_api_tokens_admin_human',
                   'admin_api_tokens_lifetime_bounded') AND contype='c'
             ORDER BY conname",
        )
        .fetch_all(pool)
        .await
        .expect("read canonical authority CHECK expressions")
    }

    async fn canonical_authority_checks(url: &str, chain: &str) -> Vec<(String, String)> {
        let db = TestDatabase::create(url, &migrator(chain).await)
            .await
            .expect("create fresh canonical compatibility fixture");
        let checks = authority_checks(db.pool()).await;
        db.close()
            .await
            .expect("drop canonical compatibility fixture");
        assert_eq!(checks.len(), 4, "{chain}: all four authority CHECKs exist");
        checks
    }

    async fn partial_authority_columns(pool: &PgPool) {
        // Deliberately nullable: refusal must roll back SET NOT NULL as well as
        // any earlier ADD CONSTRAINT/VALIDATE in the repair transaction.
        sqlx::query(
            "ALTER TABLE admin_api_tokens ADD COLUMN holder_id text, ADD COLUMN holder_kind text",
        )
        .execute(pool)
        .await
        .expect("install named columns without defaults");
    }

    fn invalid_authority_rows() -> Vec<(&'static str, Value)> {
        let mut cases = Vec::new();
        for (name, field, value) in [
            ("blank holder", "holder_id", json!("")),
            ("whitespace holder", "holder_id", json!(" \t\r\n")),
            ("unknown kind", "holder_kind", json!("robot")),
            ("AI admin", "scopes", json!(["admin"])),
            (
                "over ninety days",
                "expires_at",
                json!("2026-11-30T00:00:00.000001Z"),
            ),
            ("zero lifetime", "expires_at", json!(ISSUED)),
            (
                "negative lifetime",
                "expires_at",
                json!("2026-08-31T23:59:59Z"),
            ),
            ("infinite expiry", "expires_at", json!("infinity")),
            ("infinite issue", "issued_at", json!("-infinity")),
        ] {
            let mut invalid = row();
            invalid[field] = value;
            cases.push((name, invalid));
        }
        cases
    }

    async fn authority_write_probes(pool: &PgPool) -> Vec<String> {
        let mut failures = Vec::new();
        clear(pool).await;
        // Positive boundaries prove that blanket refusal is not enforcement.
        for (kind, scopes, expires) in [
            ("human", json!(["moderate", "admin"]), NINETY_DAYS),
            ("ai_agent", json!(["read", "moderate"]), NINETY_DAYS),
            ("human", json!(["admin"]), "2026-09-01T00:00:00.000001Z"),
        ] {
            let mut valid = row();
            valid["holder_kind"] = json!(kind);
            valid["scopes"] = scopes;
            valid["expires_at"] = json!(expires);
            if insert(pool, &valid).await.is_err() {
                failures.push(format!("valid {kind} boundary INSERT refused"));
            } else {
                // Postgres normalizes timestamp spelling, so compare authority
                // fields and exact lifetime separately instead of JSON dates.
                let actual = snapshot(pool).await;
                if actual["holder_id"] != valid["holder_id"]
                    || actual["holder_kind"] != valid["holder_kind"]
                    || actual["scopes"] != valid["scopes"]
                {
                    failures.push(format!("valid {kind} authority changed"));
                }
                let seconds: f64 = sqlx::query_scalar("SELECT EXTRACT(EPOCH FROM (expires_at - issued_at))::double precision FROM admin_api_tokens WHERE token_id=$1")
                    .bind(TOKEN).fetch_one(pool).await.expect("read accepted lifetime boundary");
                let expected = if expires == NINETY_DAYS {
                    7_776_000.0
                } else {
                    0.000001
                };
                if seconds != expected {
                    failures.push(format!("valid {kind} lifetime changed: {seconds}"));
                }
            }
            clear(pool).await;
        }
        for (name, invalid) in invalid_authority_rows() {
            if !refused(pool, &invalid, "23514").await {
                failures.push(format!("{name} INSERT accepted or wrong SQLSTATE"));
            }
        }
        for (name, statement) in [
            ("whitespace holder", "UPDATE admin_api_tokens SET holder_id=' \t\r\n' WHERE token_id=$1"),
            ("unknown kind", "UPDATE admin_api_tokens SET holder_kind='robot' WHERE token_id=$1"),
            ("AI admin", "UPDATE admin_api_tokens SET scopes='{admin}' WHERE token_id=$1"),
            ("over ninety days", "UPDATE admin_api_tokens SET expires_at=issued_at + interval '90 days 0.000001 seconds' WHERE token_id=$1"),
            ("zero lifetime", "UPDATE admin_api_tokens SET expires_at=issued_at WHERE token_id=$1"),
            ("negative lifetime", "UPDATE admin_api_tokens SET expires_at=issued_at - interval '1 second' WHERE token_id=$1"),
            ("infinite expiry", "UPDATE admin_api_tokens SET expires_at='infinity' WHERE token_id=$1"),
            ("infinite issue", "UPDATE admin_api_tokens SET issued_at='-infinity' WHERE token_id=$1"),
        ] {
            insert(pool, &row()).await.expect("insert valid update control");
            let before = snapshot(pool).await;
            let result = sqlx::query(statement).bind(TOKEN).execute(pool).await;
            let after = snapshot(pool).await;
            let rejected = result.err().is_some_and(|error| {
                error.as_database_error().and_then(|error| error.code()).as_deref() == Some("23514")
            });
            if !rejected || after != before {
                failures.push(format!("{name} UPDATE: refused={rejected}, row_preserved={}", after == before));
            }
            clear(pool).await;
        }
        failures
    }

    async fn incompatible_same_name_checks_refuse_atomically(chain: &str, shipped: i64) {
        let Some(url) = bootstrap() else { return };
        let (prefix, latest) = pre_repair_migrators(chain, shipped).await;
        let canonical = canonical_authority_checks(&url, chain).await;
        let mut failures = Vec::new();
        for (name, weak) in AUTHORITY_CHECKS {
            for variant in ["weak", "always true", "wrong kind", "NO INHERIT"] {
                for populated in [false, true] {
                    let db = TestDatabase::create(&url, &prefix)
                        .await
                        .expect("create incompatible same-name fixture");
                    partial_authority_columns(db.pool()).await;
                    let exact = &canonical
                        .iter()
                        .find(|(n, _)| n == name)
                        .expect("canonical compatibility expression")
                        .1;
                    let definition = match variant {
                        "weak" => format!("CHECK ({weak}) NOT VALID"),
                        "always true" => "CHECK (true) NOT VALID".to_owned(),
                        "wrong kind" => "UNIQUE (token_id)".to_owned(),
                        "NO INHERIT" => format!("CHECK ({exact}) NO INHERIT"),
                        _ => unreachable!("literal fixture variant"),
                    };
                    sqlx::query(sqlx::AssertSqlSafe(format!(
                        "ALTER TABLE admin_api_tokens ADD CONSTRAINT {name} {definition}"
                    )))
                    .execute(db.pool())
                    .await
                    .expect("install conflicting authority constraint");
                    if name == "admin_api_tokens_holder_named" && variant == "weak" {
                        let mut whitespace = row();
                        whitespace["holder_id"] = json!(" \t\r\n");
                        insert(db.pool(), &whitespace).await.expect(
                            "reported holder_id <> '' fixture permits whitespace authority",
                        );
                        clear(db.pool()).await;
                    }
                    if populated {
                        insert(db.pool(), &row()).await.expect(
                            "compatible named AI moderate row satisfies partial constraints",
                        );
                    }
                    let before_rows = registry_rows(db.pool()).await;
                    let before_catalog = catalog(db.pool()).await;
                    let result = latest.run(db.pool()).await;
                    let after_rows = registry_rows(db.pool()).await;
                    let after_catalog = catalog(db.pool()).await;
                    let bypasses = if result.is_ok() {
                        authority_write_probes(db.pool()).await
                    } else {
                        Vec::new()
                    };
                    db.close()
                        .await
                        .expect("drop incompatible same-name fixture");
                    if result.is_ok()
                        || after_rows != before_rows
                        || after_catalog != before_catalog
                    {
                        failures.push(format!(
                            "{name}, {variant}, populated={populated}: refused={}, rows_preserved={}, catalog_and_history_preserved={}, bypasses={bypasses:?}",
                            result.is_err(), after_rows == before_rows, after_catalog == before_catalog
                        ));
                    }
                }
            }
        }
        assert!(failures.is_empty(), "{chain}: incompatible same-name authority constraints must refuse atomically, not be trusted, replaced or partially validated; preserve nullability/defaults/constraints/indexes/rows/history: {failures:#?}");
    }

    async fn compatible_authority_checks_upgrade(chain: &str, shipped: i64) {
        let Some(url) = bootstrap() else { return };
        let (prefix, latest) = pre_repair_migrators(chain, shipped).await;
        let canonical = canonical_authority_checks(&url, chain).await;
        let mut failures = Vec::new();
        for preexisting in [false, true] {
            for populated in [false, true] {
                let db = TestDatabase::create(&url, &prefix)
                    .await
                    .expect("create compatible authority CHECK fixture");
                partial_authority_columns(db.pool()).await;
                if preexisting {
                    for (name, expression) in &canonical {
                        sqlx::query(sqlx::AssertSqlSafe(format!(
                            "ALTER TABLE admin_api_tokens ADD CONSTRAINT {name} CHECK ({expression})"
                        ))).execute(db.pool()).await.expect("install exact canonical existing CHECK");
                    }
                }
                if populated {
                    insert(db.pool(), &row())
                        .await
                        .expect("insert compatible named authority row");
                }
                let before_rows = registry_rows(db.pool()).await;
                let result = latest.run(db.pool()).await;
                let after_rows = registry_rows(db.pool()).await;
                let checks = authority_checks(db.pool()).await;
                let enforced: i64 = sqlx::query_scalar(
                    "SELECT count(*) FROM pg_constraint WHERE conrelid='admin_api_tokens'::regclass
                     AND conname IN ('admin_api_tokens_holder_named', 'admin_api_tokens_holder_kind_closed',
                     'admin_api_tokens_admin_human', 'admin_api_tokens_lifetime_bounded')
                     AND contype='c' AND convalidated AND NOT connoinherit",
                ).fetch_one(db.pool()).await.expect("count validated inheritable authority CHECKs");
                let probes = if result.is_ok() {
                    authority_write_probes(db.pool()).await
                } else {
                    Vec::new()
                };
                db.close()
                    .await
                    .expect("drop compatible authority CHECK fixture");
                if result.is_err()
                    || before_rows != after_rows
                    || checks != canonical
                    || enforced != 4
                    || !probes.is_empty()
                {
                    failures.push(format!("preexisting={preexisting}, populated={populated}: migrated={}, rows_preserved={}, exact_checks={}, enforced={enforced}, probes={probes:?}", result.is_ok(), before_rows == after_rows, checks == canonical));
                }
            }
        }
        assert!(failures.is_empty(), "{chain}: absent or exact canonical CHECKs must migrate, preserve named rows and enforce real INSERT/UPDATE authority boundaries: {failures:#?}");
    }

    async fn mirrored_authority_repairs_converge(
        first: &str,
        shipped: i64,
        second: &str,
        other_shipped: i64,
    ) {
        let Some(url) = bootstrap() else { return };
        let db = TestDatabase::create(&url, &migrator(first).await)
            .await
            .expect("create first-chain repaired database");
        insert(db.pool(), &row())
            .await
            .expect("insert compatible mirrored control");
        let before_rows = registry_rows(db.pool()).await;
        let mut before_catalog = catalog(db.pool()).await;
        // Run only the other chain's actual forward repairs, not its unrelated
        // and differently numbered predecessors. Existing history is retained.
        let (other_prefix, other_latest) = pre_repair_migrators(second, other_shipped).await;
        let mut mirror = Migrator::with_migrations(
            other_latest
                .iter()
                .filter(|m| !other_prefix.iter().any(|old| old.version == m.version))
                .cloned()
                .collect(),
        );
        mirror.set_ignore_missing(true);
        let result = mirror.run(db.pool()).await;
        let after_rows = registry_rows(db.pool()).await;
        let mut after_catalog = catalog(db.pool()).await;
        let before_history = before_catalog["migrations"]
            .as_array()
            .expect("before history");
        let after_history = after_catalog["migrations"]
            .as_array()
            .expect("after history");
        let original_history: Vec<_> = after_history
            .iter()
            .filter(|entry| {
                before_history
                    .iter()
                    .any(|old| old["version"] == entry["version"])
            })
            .collect();
        let history_preserved = original_history == before_history.iter().collect::<Vec<_>>();
        let mirror_history_recorded = mirror.iter().all(|migration| {
            after_history.iter().any(|entry| {
                entry["version"].as_i64() == Some(migration.version)
                    && entry["success"] == json!(true)
            })
        });
        before_catalog
            .as_object_mut()
            .expect("catalog object")
            .remove("migrations");
        after_catalog
            .as_object_mut()
            .expect("catalog object")
            .remove("migrations");
        let probes = if result.is_ok() {
            authority_write_probes(db.pool()).await
        } else {
            Vec::new()
        };
        db.close().await.expect("drop mirrored authority fixture");
        assert!(
            result.is_ok(),
            "{first}/{shipped} -> {second}: mirrored authority repair must succeed"
        );
        assert!(
            history_preserved,
            "mirror preserves every prior migration history row"
        );
        assert!(
            mirror_history_recorded,
            "mirror records its actual forward repair as successful"
        );
        assert_eq!(
            after_rows, before_rows,
            "mirror preserves named authority rows"
        );
        assert_eq!(
            after_catalog, before_catalog,
            "mirror converges without duplicate/replaced constraints or other schema drift"
        );
        assert!(probes.is_empty(), "{first} -> {second}: converged schema must enforce real INSERT/UPDATE controls: {probes:?}");
    }

    #[tokio::test]
    async fn cutover_incompatible_same_name_authority_checks_refuse_atomically() {
        incompatible_same_name_checks_refuse_atomically("cutover", 456).await;
    }
    #[tokio::test]
    async fn store_incompatible_same_name_authority_checks_refuse_atomically() {
        incompatible_same_name_checks_refuse_atomically("store", 457).await;
    }
    #[tokio::test]
    async fn cutover_absent_and_exact_authority_checks_upgrade() {
        compatible_authority_checks_upgrade("cutover", 456).await;
    }
    #[tokio::test]
    async fn store_absent_and_exact_authority_checks_upgrade() {
        compatible_authority_checks_upgrade("store", 457).await;
    }
    #[tokio::test]
    async fn cutover_then_store_authority_repairs_converge() {
        mirrored_authority_repairs_converge("cutover", 456, "store", 457).await;
    }
    #[tokio::test]
    async fn store_then_cutover_authority_repairs_converge() {
        mirrored_authority_repairs_converge("store", 457, "cutover", 456).await;
    }

    #[tokio::test]
    async fn cutover_partial_holder_defaults_refuse_atomically() {
        partial_holder_defaults_refuse_atomically("cutover", 456).await;
    }
    #[tokio::test]
    async fn store_partial_holder_defaults_refuse_atomically() {
        partial_holder_defaults_refuse_atomically("store", 457).await;
    }
    #[tokio::test]
    async fn cutover_partial_holders_without_defaults_upgrade() {
        partial_holders_without_defaults_upgrade("cutover", 456).await;
    }
    #[tokio::test]
    async fn store_partial_holders_without_defaults_upgrade() {
        partial_holders_without_defaults_upgrade("store", 457).await;
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

    mod no_temp_contract {
        use super::*;
        use sqlx::postgres::PgPoolOptions;

        // Break caught: a forward authority repair starts requiring TEMPORARY,
        // or an error unrelated to authority is mistaken for safe refusal.
        fn sqlstate(error: &(dyn std::error::Error + 'static)) -> String {
            let mut source = Some(error);
            while let Some(error) = source {
                if let Some(error) = error.downcast_ref::<sqlx::Error>() {
                    if let Some(code) = error.as_database_error().and_then(|e| e.code()) {
                        return code.into_owned();
                    }
                }
                if let Some(error) = error.downcast_ref::<sqlx::postgres::PgDatabaseError>() {
                    return error.code().to_owned();
                }
                source = error.source();
            }
            "non-database failure".to_owned()
        }

        async fn no_helper_inventory(pool: &PgPool) -> Result<Value, sqlx::Error> {
            // Include pg_temp, other schemas and functions, not just public
            // tables. Compare before probes, on the same role's live pool.
            sqlx::query_scalar(
                "SELECT jsonb_build_object(
                 'schemas', (SELECT jsonb_agg(nspname ORDER BY nspname) FROM pg_namespace WHERE nspname <> 'information_schema' AND nspname !~ '^pg_(catalog|toast)'),
                 'relations', (SELECT jsonb_agg(jsonb_build_array(n.nspname,c.relname,c.relkind,c.relpersistence) ORDER BY n.nspname,c.relname) FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname <> 'information_schema' AND n.nspname !~ '^pg_(catalog|toast)' AND c.relkind NOT IN ('i','I')),
                 'functions', (SELECT jsonb_agg(jsonb_build_array(n.nspname,p.proname,pg_get_function_identity_arguments(p.oid)) ORDER BY n.nspname,p.proname,p.oid) FROM pg_proc p JOIN pg_namespace n ON n.oid=p.pronamespace WHERE n.nspname <> 'information_schema' AND n.nspname !~ '^pg_(catalog|toast)'))",
            ).fetch_one(pool).await
        }

        async fn assert_limited_role(pool: &PgPool) -> Result<(), String> {
            let actual: (bool, bool, bool, bool, bool, bool) = sqlx::query_as(
                "SELECT current_user = session_user, r.rolsuper, r.rolcreatedb,
                 r.rolcreaterole, has_database_privilege(current_user,current_database(),'TEMPORARY'),
                 NOT EXISTS (SELECT 1 FROM pg_auth_members WHERE member=r.oid)
                 FROM pg_roles r WHERE r.rolname=current_user",
            ).fetch_one(pool).await.map_err(|e| format!("role verification: {}", sqlstate(&e)))?;
            if actual != (true, false, false, false, false, true) {
                return Err(format!("fixture must really be a non-superuser without effective TEMPORARY: {actual:?}"));
            }
            let grants: (bool, bool) = sqlx::query_as(
                "SELECT has_database_privilege(current_user,current_database(),'CONNECT'),
                 has_database_privilege(current_user,current_database(),'CREATE')",
            )
            .fetch_one(pool)
            .await
            .map_err(|e| format!("contract grants: {}", sqlstate(&e)))?;
            if grants != (true, true) {
                return Err(format!(
                    "missing existing CONNECT,CREATE fixture contract: {grants:?}"
                ));
            }
            Ok(())
        }

        async fn check_behavior(pool: &PgPool) -> Vec<String> {
            let mut failures = authority_write_probes(pool).await;
            let omissions = omitted_holder_inserts(pool).await;
            if omissions.iter().any(|(_, code)| code != "23502") {
                failures.push(format!(
                    "omitted holders must fail NOT NULL, not permissions: {omissions:?}"
                ));
            }
            let mut uppercase = row();
            uppercase["holder_kind"] = json!("HUMAN");
            uppercase["scopes"] = json!(["admin"]);
            if !refused(pool, &uppercase, "23514").await {
                failures.push("uppercase HUMAN admin INSERT must fail CHECK 23514".to_owned());
            }
            let mut human = row();
            human["holder_kind"] = json!("human");
            human["scopes"] = json!(["admin"]);
            insert(pool, &human)
                .await
                .expect("valid human admin UPDATE control");
            let before = snapshot(pool).await;
            let result =
                sqlx::query("UPDATE admin_api_tokens SET holder_kind='HUMAN' WHERE token_id=$1")
                    .bind(TOKEN)
                    .execute(pool)
                    .await;
            let after = snapshot(pool).await;
            if result.as_ref().err().map(|e| sqlstate(e)).as_deref() != Some("23514")
                || after != before
            {
                failures.push(
                    "uppercase HUMAN admin UPDATE must fail CHECK 23514 without changing row"
                        .to_owned(),
                );
            }
            clear(pool).await;
            failures
        }

        async fn exercise(
            chain: &str,
            shipped: i64,
            case: &str,
            mirror_chain: Option<(&str, i64)>,
        ) {
            let Some(url) = bootstrap() else { return };
            let (prefix, latest) = pre_repair_migrators(chain, shipped).await;
            // Existing helper captures same-server CHECK spelling for compatible
            // setup only; literal invalid INSERT/UPDATE probes are the oracle.
            let canonical = canonical_authority_checks(&url, chain).await;
            let empty = Migrator::with_migrations(Vec::new());
            let db = TestDatabase::create(&url, &empty)
                .await
                .expect("create disposable no-TEMP fixture");
            let name: String = sqlx::query_scalar("SELECT current_database()")
                .fetch_one(db.pool())
                .await
                .expect("read owned fixture name");
            let role = format!(
                "admin_no_temp_{}",
                &hex::encode(sha2::Sha256::digest(name.as_bytes()))[..24]
            );
            let created = sqlx::query(sqlx::AssertSqlSafe(format!(
                "CREATE ROLE {role} LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOINHERIT NOREPLICATION NOBYPASSRLS"
            ))).execute(db.pool()).await;
            if let Err(error) = created {
                let code = sqlstate(&error);
                db.close()
                    .await
                    .expect("drop unprovisioned no-TEMP fixture");
                panic!("fixture role provisioning failed ({code}); this is not migration RED");
            }
            let mut limited = None;
            // Always release the role and database before asserting on RED.
            let outcome: Result<Vec<String>, String> = async {
                for statement in [
                    format!("REVOKE ALL ON DATABASE \"{name}\" FROM PUBLIC"),
                    format!("GRANT CONNECT,CREATE ON DATABASE \"{name}\" TO {role}"),
                    format!("GRANT USAGE,CREATE ON SCHEMA public TO {role}"),
                    // Even an empty Migrator initializes this ledger as the
                    // bootstrap principal; its ownership belongs to migrator.
                    format!("ALTER TABLE public._sqlx_migrations OWNER TO {role}"),
                ] {
                    sqlx::query(sqlx::AssertSqlSafe(statement)).execute(db.pool()).await
                        .map_err(|e| format!("disposable fixture grants: {}", sqlstate(&e)))?;
                }
                // Trust-auth login on the guarded disposable service: neither
                // session_user nor current_user is the bootstrap superuser.
                let pool = PgPoolOptions::new().max_connections(1)
                    .connect_with((*db.pool().connect_options()).clone().username(&role)).await
                    .map_err(|e| format!("limited fixture connection: {}", sqlstate(&e)))?;
                limited = Some(pool.clone());
                assert_limited_role(&pool).await?;
                prefix.run(&pool).await.map_err(|e| format!("predecessor setup: {} (not intended authority refusal)", sqlstate(&e)))?;
                if case != "empty" {
                    partial_authority_columns(&pool).await;
                    if case == "named canonical" {
                        for (name, expression) in &canonical {
                            sqlx::query(sqlx::AssertSqlSafe(format!(
                                "ALTER TABLE admin_api_tokens ADD CONSTRAINT {name} CHECK ({expression}) NOT VALID"
                            ))).execute(&pool).await.map_err(|e| format!("canonical fixture CHECK: {}", sqlstate(&e)))?;
                        }
                    } else if case == "weak same-name CHECK" {
                        sqlx::query("ALTER TABLE admin_api_tokens ADD CONSTRAINT admin_api_tokens_holder_named CHECK (true) NOT VALID")
                            .execute(&pool).await.map_err(|e| format!("weak fixture CHECK: {}", sqlstate(&e)))?;
                    }
                    let mut human = row();
                    human["holder_kind"] = json!("human");
                    human["scopes"] = json!(["moderate", "admin"]);
                    if case == "missing holder" {
                        human["holder_id"] = Value::Null;
                    } else if case == "AI admin" {
                        human["holder_kind"] = json!("ai_agent");
                    }
                    insert(&pool, &human).await.map_err(|e| format!("populate authority fixture: {}", sqlstate(&e)))?;
                    let mut ai = row();
                    ai["token_id"] = json!("adm_fedcba9876543210");
                    ai["holder_id"] = json!("fixture-ai-two");
                    ai["secret_hash"] = json!(hex::encode(sha2::Sha256::digest(b"second-synthetic-no-temp-row")));
                    insert(&pool, &ai).await.map_err(|e| format!("populate AI non-admin control: {}", sqlstate(&e)))?;
                }
                let before_rows = registry_rows(&pool).await;
                let before_catalog = catalog(&pool).await;
                let before_helpers = no_helper_inventory(&pool).await.map_err(|e| format!("before inventory: {}", sqlstate(&e)))?;
                assert_limited_role(&pool).await?;
                let result = latest.run(&pool).await;
                let after_rows = registry_rows(&pool).await;
                let after_catalog = catalog(&pool).await;
                let after_helpers = no_helper_inventory(&pool).await.map_err(|e| format!("after inventory: {}", sqlstate(&e)))?;
                assert_limited_role(&pool).await?;
                let mut failures = Vec::new();
                let invalid = matches!(case, "missing holder" | "AI admin" | "weak same-name CHECK");
                if invalid {
                    let code = result.as_ref().err().map(|e| sqlstate(e));
                    if !matches!(code.as_deref(), Some("P0001" | "23502" | "23514")) {
                        failures.push(format!("authority must refuse, not succeed or fail permissions/setup: {code:?}"));
                    }
                    if before_catalog != after_catalog {
                        failures.push("authority refusal changed columns/constraints/indexes/migration history".to_owned());
                    }
                } else if let Err(error) = &result {
                    failures.push(format!("compatible upgrade requires only CONNECT,CREATE, not TEMPORARY; got {}", sqlstate(error)));
                } else {
                    let checks = authority_checks(&pool).await;
                    let validated: i64 = sqlx::query_scalar(
                        "SELECT count(*) FROM pg_constraint WHERE conrelid='admin_api_tokens'::regclass
                         AND conname IN ('admin_api_tokens_holder_named','admin_api_tokens_holder_kind_closed',
                         'admin_api_tokens_admin_human','admin_api_tokens_lifetime_bounded')
                         AND contype='c' AND convalidated AND NOT connoinherit",
                    ).fetch_one(&pool).await.map_err(|e| format!("validate catalog: {}", sqlstate(&e)))?;
                    if checks != canonical || validated != 4 {
                        failures.push(format!("real canonical CHECKs must remain exact and validated: count={validated}, exact={}", checks == canonical));
                    }
                    failures.extend(check_behavior(&pool).await);
                }
                if before_rows != after_rows {
                    failures.push("migration changed explicit holder rows".to_owned());
                }
                if before_helpers != after_helpers {
                    failures.push("migration left a helper schema/relation/function or temporary object".to_owned());
                }
                // Restore the seeded rows after behavior probes before checking
                // mirror convergence. This is fixture data, not real issuance.
                if result.is_ok() && !invalid {
                    sqlx::query("DELETE FROM admin_api_tokens").execute(&pool).await
                        .map_err(|e| format!("clear probe fixture: {}", sqlstate(&e)))?;
                    for value in before_rows.as_array().expect("row array") {
                        insert(&pool, value).await.map_err(|e| format!("restore seeded rows: {}", sqlstate(&e)))?;
                    }
                    if let Some((other, other_shipped)) = mirror_chain {
                        let (other_prefix, other_latest) = pre_repair_migrators(other, other_shipped).await;
                        let mut mirror = Migrator::with_migrations(other_latest.iter()
                            .filter(|m| !other_prefix.iter().any(|p| p.version == m.version)).cloned().collect());
                        mirror.set_ignore_missing(true);
                        let mut before = catalog(&pool).await;
                        let prior_history = before["migrations"].clone();
                        let mirrored = mirror.run(&pool).await;
                        assert_limited_role(&pool).await?;
                        let mut after = catalog(&pool).await;
                        let history = after["migrations"].as_array().expect("mirror history");
                        let preserved: Vec<_> = history.iter().filter(|entry| prior_history.as_array().expect("prior history").iter()
                            .any(|old| old["version"] == entry["version"])).cloned().collect();
                        let recorded = mirror.iter().all(|m| history.iter().any(|entry|
                            entry["version"].as_i64() == Some(m.version) && entry["success"] == json!(true)));
                        before.as_object_mut().expect("catalog").remove("migrations");
                        after.as_object_mut().expect("catalog").remove("migrations");
                        if mirrored.is_err() || before != after || json!(preserved) != prior_history || !recorded
                            || registry_rows(&pool).await != before_rows {
                            failures.push(format!("{chain} -> {other}: mirror failed/convergence drift; SQLSTATE={:?}", mirrored.as_ref().err().map(|e| sqlstate(e))));
                        }
                        if mirrored.is_ok() {
                            let before_repeat = catalog(&pool).await;
                            let repeated = mirror.run(&pool).await;
                            if repeated.is_err() || catalog(&pool).await != before_repeat {
                                failures.push("repeat mirror was not history/schema-idempotent".to_owned());
                            }
                            failures.extend(check_behavior(&pool).await);
                        }
                        if no_helper_inventory(&pool).await.map_err(|e| format!("mirror inventory: {}", sqlstate(&e)))? != before_helpers {
                            failures.push("mirror left helper/temporary residue".to_owned());
                        }
                        assert_limited_role(&pool).await?;
                    }
                }
                Ok(failures)
            }.await;
            if let Some(pool) = limited {
                pool.close().await;
            }
            // Role owns only objects in this disposable database, never the DB
            // itself. Reassign here before dropping the fixture-local grants.
            for statement in [
                format!("REASSIGN OWNED BY {role} TO CURRENT_USER"),
                format!("DROP OWNED BY {role}"),
                format!("DROP ROLE {role}"),
            ] {
                sqlx::query(sqlx::AssertSqlSafe(statement))
                    .execute(db.pool())
                    .await
                    .expect(
                        "clean disposable role and its grants; cleanup failure is not intended RED",
                    );
            }
            db.close()
                .await
                .expect("drop no-TEMP database; cleanup must succeed");
            let failures = outcome.unwrap_or_else(|reason| {
                panic!("{chain}/{case}: fixture/setup limitation, not intended RED: {reason}")
            });
            assert!(
                failures.is_empty(),
                "{chain}/{case}: no-TEMP migration contract: {failures:#?}"
            );
        }

        #[tokio::test]
        async fn cutover_no_temp_empty_upgrade() {
            exercise("cutover", 456, "empty", None).await;
        }
        #[tokio::test]
        async fn store_no_temp_empty_upgrade() {
            exercise("store", 457, "empty", None).await;
        }
        #[tokio::test]
        async fn cutover_no_temp_named_canonical_upgrade() {
            exercise("cutover", 456, "named canonical", None).await;
        }
        #[tokio::test]
        async fn store_no_temp_named_canonical_upgrade() {
            exercise("store", 457, "named canonical", None).await;
        }
        #[tokio::test]
        async fn cutover_no_temp_missing_holder_refuses_atomically() {
            exercise("cutover", 456, "missing holder", None).await;
        }
        #[tokio::test]
        async fn store_no_temp_missing_holder_refuses_atomically() {
            exercise("store", 457, "missing holder", None).await;
        }
        #[tokio::test]
        async fn cutover_no_temp_ai_admin_refuses_atomically() {
            exercise("cutover", 456, "AI admin", None).await;
        }
        #[tokio::test]
        async fn store_no_temp_ai_admin_refuses_atomically() {
            exercise("store", 457, "AI admin", None).await;
        }
        #[tokio::test]
        async fn cutover_no_temp_weak_named_check_refuses_atomically() {
            exercise("cutover", 456, "weak same-name CHECK", None).await;
        }
        #[tokio::test]
        async fn store_no_temp_weak_named_check_refuses_atomically() {
            exercise("store", 457, "weak same-name CHECK", None).await;
        }
        // Break caught: deriving the reference from search_path/public instead
        // of the locked registry namespace, misquoting that namespace, or
        // replacing a preexisting helper on collision. The original no-TEMP
        // matrix above independently covers limited-role success/refusal/mirrors.
        async fn quoted_namespace_reference(chain: &str, shipped: i64, collision: bool) {
            let Some(url) = bootstrap() else { return };
            let (prefix, mut latest) = pre_repair_migrators(chain, shipped).await;
            // Pin sqlx bookkeeping to the real public history. This pool runs
            // quoted-first, and CREATE TABLE IF NOT EXISTS checks existence in
            // the creation schema only: an unqualified history table would be
            // shadowed into the registry schema, read back empty, and make
            // sqlx re-apply the whole chain. The repair SQL itself still
            // resolves every product object through the quoted namespace.
            latest.dangerous_set_table_name("public._sqlx_migrations");
            let db = TestDatabase::create(&url, &prefix)
                .await
                .expect("create isolated reference-namespace fixture");
            sqlx::raw_sql(
                r#"CREATE SCHEMA "authority ""fixture";
                   ALTER TABLE public.admin_api_tokens SET SCHEMA "authority ""fixture";
                   CREATE TABLE public.admin_api_token_authority_contract (marker TEXT);
                   INSERT INTO public.admin_api_token_authority_contract VALUES ('public-decoy');"#,
            )
            .execute(db.pool())
            .await
            .expect("prepare quoted registry namespace and unrelated public decoy");
            let pool = PgPoolOptions::new()
                .max_connections(1)
                .after_connect(|connection, _| {
                    Box::pin(async move {
                        sqlx::query(r#"SET search_path TO "authority ""fixture", public"#)
                            .execute(connection)
                            .await?;
                        Ok(())
                    })
                })
                .connect_with((*db.pool().connect_options()).clone())
                .await
                .expect("connect only to owned disposable fixture");
            if collision {
                sqlx::raw_sql(
                    r#"CREATE TABLE "authority ""fixture".admin_api_token_authority_contract (marker TEXT);
                       INSERT INTO "authority ""fixture".admin_api_token_authority_contract VALUES ('existing-helper');"#,
                ).execute(&pool).await.expect("prepare existing schema-local helper");
            }
            partial_authority_columns(&pool).await;
            insert(&pool, &row())
                .await
                .expect("seed explicit named holder");
            let before_rows = registry_rows(&pool).await;
            let before_helpers = no_helper_inventory(&pool)
                .await
                .expect("before reference inventory");
            let namespace_catalog = || {
                sqlx::query_scalar::<_, Value>(
                "SELECT jsonb_build_object(
                 'columns', (SELECT jsonb_agg(jsonb_build_array(a.attname,a.atttypid,a.atttypmod,a.attcollation,a.attnotnull,a.atthasdef,pg_get_expr(d.adbin,d.adrelid)) ORDER BY a.attnum) FROM pg_attribute a LEFT JOIN pg_attrdef d ON d.adrelid=a.attrelid AND d.adnum=a.attnum WHERE a.attrelid='admin_api_tokens'::regclass AND a.attnum>0 AND NOT a.attisdropped),
                 'constraints', (SELECT jsonb_agg(jsonb_build_array(conname,pg_get_constraintdef(oid),convalidated) ORDER BY conname) FROM pg_constraint WHERE conrelid='admin_api_tokens'::regclass),
                 'indexes', (SELECT jsonb_agg(pg_get_indexdef(indexrelid) ORDER BY indexrelid) FROM pg_index WHERE indrelid='admin_api_tokens'::regclass),
                 'migrations', (SELECT jsonb_agg(to_jsonb(m) ORDER BY version) FROM _sqlx_migrations m))",
            )
            };
            let before_catalog = namespace_catalog()
                .fetch_one(&pool)
                .await
                .expect("before namespace catalog");
            let result = latest.run(&pool).await;
            let after_catalog = namespace_catalog()
                .fetch_one(&pool)
                .await
                .expect("after namespace catalog");
            let after_rows = registry_rows(&pool).await;
            let mut failures = Vec::new();
            if after_rows != before_rows {
                failures.push("migration changed explicit holder data".to_owned());
            }
            if collision {
                if result.as_ref().err().map(|e| sqlstate(e)).as_deref() != Some("42P07") {
                    failures.push("helper collision must refuse duplicate_table 42P07".to_owned());
                }
                if before_catalog != after_catalog {
                    failures
                        .push("collision changed registry catalog or migration history".to_owned());
                }
                let markers: Vec<String> = sqlx::query_scalar(
                    r#"SELECT marker FROM "authority ""fixture".admin_api_token_authority_contract"#,
                ).fetch_all(&pool).await.expect("existing helper must not disappear");
                if markers != ["existing-helper"] {
                    failures
                        .push("collision replaced or changed preexisting helper rows".to_owned());
                }
            } else if result.is_err() {
                failures.push(format!(
                    "quoted registry namespace must migrate: {:?}",
                    result.as_ref().err().map(|e| sqlstate(e))
                ));
            } else {
                failures.extend(check_behavior(&pool).await);
            }
            if no_helper_inventory(&pool)
                .await
                .expect("after reference inventory")
                != before_helpers
            {
                failures.push(
                    "reference leaked a relation/schema/function or consumed an unrelated object"
                        .to_owned(),
                );
            }
            let decoys: Vec<String> =
                sqlx::query_scalar("SELECT marker FROM public.admin_api_token_authority_contract")
                    .fetch_all(&pool)
                    .await
                    .expect("unrelated public helper must remain");
            if decoys != ["public-decoy"] {
                failures.push("reference touched unrelated public helper data".to_owned());
            }
            pool.close().await;
            db.close().await.expect("drop reference-namespace fixture");
            assert!(
                failures.is_empty(),
                "{chain}/collision={collision}: {failures:#?}"
            );
        }

        #[tokio::test]
        async fn cutover_reference_uses_quoted_registry_namespace_not_public_decoy() {
            quoted_namespace_reference("cutover", 456, false).await;
        }
        #[tokio::test]
        async fn store_reference_uses_quoted_registry_namespace_not_public_decoy() {
            quoted_namespace_reference("store", 457, false).await;
        }
        #[tokio::test]
        async fn cutover_reference_collision_preserves_existing_object_and_registry() {
            quoted_namespace_reference("cutover", 456, true).await;
        }
        #[tokio::test]
        async fn store_reference_collision_preserves_existing_object_and_registry() {
            quoted_namespace_reference("store", 457, true).await;
        }

        #[tokio::test]
        async fn cutover_then_store_no_temp_mirror_is_idempotent() {
            exercise("cutover", 456, "named canonical", Some(("store", 457))).await;
        }
        #[tokio::test]
        async fn store_then_cutover_no_temp_mirror_is_idempotent() {
            exercise("store", 457, "named canonical", Some(("cutover", 456))).await;
        }
    }
}
