//! Real SQLx interoperability proof for the staging migration runner (TOG-11572).
//! Only agent-testdb or the ephemeral CI service, never an application URL.
//! Controller: python3 scripts/cargo_cache.py run -- test -p two-bot-cutover
//! --test staging_migrate_db -- --ignored

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;
use sqlx::{
    postgres::{PgConnectOptions, PgSslMode},
    Connection, PgConnection,
};

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

fn url_allowed(url: &str, github_actions: bool) -> bool {
    url == "postgres://agent_test@agent-testdb:5432/agent_test"
        || (github_actions && url == "postgres://agent_test@127.0.0.1:5432/agent_test")
}

fn admin_options(host: &str, database: &str) -> PgConnectOptions {
    PgConnectOptions::new_without_pgpass()
        .host(host)
        .port(5432)
        .username("agent_test")
        .password("")
        .database(database)
        .ssl_mode(PgSslMode::Disable)
}

struct Fixture {
    admin: PgConnection,
    host: String,
    database: String,
    outsider: String,
}

impl Fixture {
    async fn new() -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let url = std::env::var("TWO_TEST_DATABASE_URL")?;
        let ci = std::env::var("GITHUB_ACTIONS").as_deref() == Ok("true");
        assert!(url_allowed(&url, ci), "non-test database refused");
        let host = if url.contains("@127.0.0.1:") {
            "127.0.0.1"
        } else {
            "agent-testdb"
        };
        let mut admin = PgConnection::connect_with(&admin_options(host, "agent_test")).await?;
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let database = format!("two_bot_staging_fx_{}_{nonce}", std::process::id());
        let outsider = format!("stg_fx_outsider_{}_{nonce}", std::process::id());
        // Cluster-level group shared with other suites: tolerate a concurrent create.
        sqlx::query(
            "DO $$ BEGIN CREATE ROLE two_bot_migrator NOLOGIN; \
             EXCEPTION WHEN duplicate_object OR unique_violation THEN NULL; END $$",
        )
        .execute(&mut admin)
        .await?;
        for sql in [
            format!("CREATE DATABASE {database}"),
            format!("CREATE ROLE {outsider} LOGIN"),
        ] {
            sqlx::query(sqlx::AssertSqlSafe(sql))
                .execute(&mut admin)
                .await?;
        }
        let mut scratch = PgConnection::connect_with(&admin_options(host, &database)).await?;
        sqlx::query("ALTER SCHEMA public OWNER TO two_bot_migrator")
            .execute(&mut scratch)
            .await?;
        scratch.close().await?;
        Ok(Self {
            admin,
            host: host.to_owned(),
            database,
            outsider,
        })
    }

    fn url(&self, user: &str, database: &str) -> String {
        format!(
            "postgres://{user}@{}:5432/{database}?sslmode=disable",
            self.host
        )
    }

    async fn run(
        &self,
        mode: &str,
        url: Option<String>,
        host: &str,
        db: &str,
    ) -> (i32, String, String) {
        let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_staging-migrate"));
        cmd.args([
            mode,
            "--source-sha",
            SHA,
            "--staging-host",
            host,
            "--staging-database",
            db,
            "--recovery-evidence-ref",
            "TOG-fixture#recovery",
            "--acl-plan-ref",
            "TOG-fixture#acl",
        ])
        .env_remove("TWO_BOT_STAGING_MIGRATOR_DATABASE_URL");
        for key in ["PGOPTIONS", "PGPASSFILE", "PGSERVICE"] {
            cmd.env_remove(key);
        }
        if let Some(url) = url {
            cmd.env("TWO_BOT_STAGING_MIGRATOR_DATABASE_URL", url);
        }
        let out = cmd.output().await.expect("spawn staging-migrate");
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    async fn scratch(&self) -> Result<PgConnection, sqlx::Error> {
        PgConnection::connect_with(&admin_options(&self.host, &self.database)).await
    }

    async fn finish(mut self) -> TestResult {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP DATABASE {} WITH (FORCE)",
            self.database
        )))
        .execute(&mut self.admin)
        .await?;
        sqlx::query(sqlx::AssertSqlSafe(format!("DROP ROLE {}", self.outsider)))
            .execute(&mut self.admin)
            .await?;
        self.admin.close().await?;
        Ok(())
    }
}

fn manifest(stdout: &str) -> Value {
    serde_json::from_str(stdout).expect("manifest json")
}

#[tokio::test]
#[ignore = "requires agent-testdb or the CI service"]
async fn real_sqlx_runner_cases() -> TestResult {
    let fx = Fixture::new().await?;
    let migrator = fx.url("agent_test", &fx.database);
    let (host, db) = (fx.host.clone(), fx.database.clone());
    let total = two_bot_cutover::staging_migrate::MIGRATOR.iter().count() as u64;

    // Missing binding and wrong target refuse before any DDL.
    let (code, _, err) = fx.run("--apply", None, &host, &db).await;
    assert_eq!(code, 2, "{err}");
    let (code, _, _) = fx
        .run(
            "--apply",
            Some(migrator.clone()),
            &host,
            "two_bot_staging_other",
        )
        .await;
    assert_eq!(code, 2);
    let (code, _, _) = fx
        .run("--apply", Some(migrator.clone()), "other-host", &db)
        .await;
    assert_eq!(code, 2);
    // A login that cannot SET ROLE two_bot_migrator is refused.
    let outsider = fx.url(&fx.outsider, &fx.database);
    let (code, _, err) = fx.run("--apply", Some(outsider), &host, &db).await;
    assert_eq!(code, 2, "{err}");
    let mut c = fx.scratch().await?;
    let ledger: bool =
        sqlx::query_scalar("SELECT to_regclass('public._sqlx_migrations') IS NOT NULL")
            .fetch_one(&mut c)
            .await?;
    assert!(!ledger, "refusals must not create the ledger");

    // Plan is read-only.
    let (code, out, err) = fx.run("--plan", Some(migrator.clone()), &host, &db).await;
    assert_eq!(code, 0, "{err}");
    assert_eq!(manifest(&out)["applied_count"], 0);
    let ledger: bool =
        sqlx::query_scalar("SELECT to_regclass('public._sqlx_migrations') IS NOT NULL")
            .fetch_one(&mut c)
            .await?;
    assert!(!ledger, "plan must not create the ledger");

    // Initial apply.
    let (code, out, err) = fx.run("--apply", Some(migrator.clone()), &host, &db).await;
    assert_eq!(code, 0, "{err}");
    let m = manifest(&out);
    assert_eq!(m["applied_count"], total);
    assert!(m["role_verified_connections"].as_u64().unwrap() >= 1);
    assert_eq!(m["ledger_after"].as_array().unwrap().len() as u64, total);
    assert!(!out.contains("postgres://"), "manifest must not echo URLs");
    let owner: String = sqlx::query_scalar(
        "SELECT pg_get_userbyid(relowner)::text FROM pg_class \
         WHERE oid = to_regclass('public._sqlx_migrations')",
    )
    .fetch_one(&mut c)
    .await?;
    assert_eq!(owner, "two_bot_migrator");
    let foreign: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = 'public' AND c.relkind IN ('r','S') \
         AND pg_get_userbyid(c.relowner) <> 'two_bot_migrator'",
    )
    .fetch_one(&mut c)
    .await?;
    assert_eq!(foreign, 0, "every DDL connection ran as the migrator");

    // Repeat is a no-op.
    let (code, out, err) = fx.run("--apply", Some(migrator.clone()), &host, &db).await;
    assert_eq!(code, 0, "{err}");
    assert_eq!(manifest(&out)["applied_count"], 0);

    // SHA-384 drift refuses.
    sqlx::query("UPDATE public._sqlx_migrations SET checksum = decode(repeat('00', 48), 'hex') WHERE version = (SELECT min(version) FROM public._sqlx_migrations)")
        .execute(&mut c)
        .await?;
    let (code, _, err) = fx.run("--apply", Some(migrator.clone()), &host, &db).await;
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("drift"));
    // Restore the checksum from the source, then an incomplete row refuses.
    let (code, _, _) = fx.run("--plan", Some(migrator.clone()), &host, &db).await;
    assert_eq!(code, 2);
    sqlx::query("DELETE FROM public._sqlx_migrations WHERE version = (SELECT max(version) FROM public._sqlx_migrations)")
        .execute(&mut c)
        .await?;
    // Re-apply the removed tail only after fixing drift is out of scope; instead mark one failed.
    sqlx::query("UPDATE public._sqlx_migrations SET success = false WHERE version = (SELECT max(version) FROM public._sqlx_migrations)")
        .execute(&mut c)
        .await?;
    // Undo the drift so only the incomplete row is under test.
    for m in two_bot_cutover::staging_migrate::MIGRATOR.iter() {
        sqlx::query("UPDATE public._sqlx_migrations SET checksum = $2 WHERE version = $1")
            .bind(m.version)
            .bind(m.checksum.as_ref())
            .execute(&mut c)
            .await?;
    }
    let (code, _, err) = fx.run("--apply", Some(migrator), &host, &db).await;
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("incomplete"));

    c.close().await?;
    fx.finish().await
}
