//! Real SQLx interoperability proof for the staging migration runner.
//! Only agent-testdb or the ephemeral CI service, never an application URL.
//! Controller: python3 scripts/cargo_cache.py run -- test -p two-bot-cutover
//! --test staging_migrate_db -- --ignored
//!
//! Shape: seed the exact 29-version staging ledger through a filtered SQLx
//! migrator as a non-superuser member login under SET ROLE, then run the real
//! `staging-migrate apply` with the reviewed `expected_pending` list through
//! the full migrator. The seeded max is 390 while most pending versions sort
//! below it, so a full ledger afterwards proves SQLx applies every unapplied
//! version regardless of the ledger max.

use std::{
    collections::HashSet,
    time::{SystemTime, UNIX_EPOCH},
};

use serde_json::Value;
use sqlx::{
    migrate::Migrator,
    postgres::{PgConnectOptions, PgPoolOptions, PgSslMode},
    Connection, Executor, PgConnection,
};

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

/// Exact staging ledger versions at the base SHA (29 rows, max 390).
const SEED_VERSIONS: [i64; 29] = [
    1, 2, 120, 121, 122, 123, 140, 141, 150, 160, 170, 180, 190, 200, 210, 300, 310, 311, 320, 330,
    331, 332, 333, 334, 340, 350, 351, 360, 390,
];

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
    mlogin: String,
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
        let pid = std::process::id();
        let database = format!("two_bot_staging_fx_{pid}_{nonce}");
        let outsider = format!("stg_fx_outsider_{pid}_{nonce}");
        let mlogin = format!("stg_fx_mlogin_{pid}_{nonce}");
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
            // Non-superuser member login: proves SET ROLE works by membership,
            // not by superuser powers.
            format!("CREATE ROLE {mlogin} LOGIN"),
        ] {
            sqlx::query(sqlx::AssertSqlSafe(sql))
                .execute(&mut admin)
                .await?;
        }
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "GRANT CONNECT ON DATABASE {database} TO two_bot_migrator"
        )))
        .execute(&mut admin)
        .await?;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "GRANT two_bot_migrator TO {mlogin}"
        )))
        .execute(&mut admin)
        .await?;
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
            mlogin,
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
        expected_pending: Option<&str>,
        plan_binding: Option<(&str, &str)>,
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
        ]);
        if let Some(list) = expected_pending {
            cmd.args(["--expected-pending", list]);
        }
        if let Some((hash, run_id)) = plan_binding {
            cmd.args(["--plan-manifest-sha256", hash, "--plan-run-id", run_id]);
        }
        cmd.env_remove("TWO_BOT_STAGING_MIGRATOR_DATABASE_URL");
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
        for role in [&self.outsider, &self.mlogin] {
            sqlx::query(sqlx::AssertSqlSafe(format!("DROP ROLE {role}")))
                .execute(&mut self.admin)
                .await?;
        }
        self.admin.close().await?;
        Ok(())
    }
}

fn manifest(stdout: &str) -> Value {
    serde_json::from_str(stdout).expect("manifest json")
}

fn pending_list(value: &Value) -> Vec<i64> {
    value
        .as_array()
        .expect("pending list")
        .iter()
        .map(|v| v.as_i64().expect("version"))
        .collect()
}

/// Seed the 29-version staging state through a filtered SQLx migrator, as the
/// non-superuser member login with SET ROLE on the single pooled connection.
async fn seed_staging_state(fx: &Fixture) -> TestResult {
    let options = PgConnectOptions::new_without_pgpass()
        .host(&fx.host)
        .port(5432)
        .username(&fx.mlogin)
        .password("")
        .database(&fx.database)
        .ssl_mode(PgSslMode::Disable);
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .after_connect(|conn: &mut PgConnection, _meta| {
            Box::pin(async move {
                conn.execute("SET ROLE two_bot_migrator").await?;
                Ok::<_, sqlx::Error>(())
            })
        })
        .connect_with(options)
        .await?;
    let seed: Vec<_> = two_bot_cutover::staging_migrate::MIGRATOR
        .iter()
        .filter(|m| SEED_VERSIONS.contains(&m.version))
        .cloned()
        .collect();
    assert_eq!(
        seed.len(),
        SEED_VERSIONS.len(),
        "seed must cover 29 versions"
    );
    Migrator::with_migrations(seed).run(&pool).await?;
    pool.close().await;
    Ok(())
}

#[tokio::test]
#[ignore = "requires agent-testdb or the CI service"]
async fn real_sqlx_runner_cases() -> TestResult {
    let fx = Fixture::new().await?;
    let migrator = fx.url("agent_test", &fx.database);
    let member = fx.url(&fx.mlogin, &fx.database);
    let (host, db) = (fx.host.clone(), fx.database.clone());
    let total = two_bot_cutover::staging_migrate::MIGRATOR.iter().count() as u64;

    // The member login is a plain non-superuser that holds the migrator group
    // by membership only.
    let mut c = fx.scratch().await?;
    let superuser: bool = sqlx::query_scalar("SELECT rolsuper FROM pg_roles WHERE rolname = $1")
        .bind(&fx.mlogin)
        .fetch_one(&mut c)
        .await?;
    assert!(!superuser, "member login must not be a superuser");
    let is_member: bool =
        sqlx::query_scalar("SELECT pg_has_role($1, 'two_bot_migrator', 'MEMBER')")
            .bind(&fx.mlogin)
            .fetch_one(&mut c)
            .await?;
    assert!(is_member, "member login must hold the migrator group");

    // Missing binding and wrong target refuse before any DDL.
    let (code, _, err) = fx.run("--apply", None, &host, &db, Some(""), None).await;
    assert_eq!(code, 2, "{err}");
    let (code, _, _) = fx
        .run(
            "--apply",
            Some(migrator.clone()),
            &host,
            "two_bot_staging_other",
            Some(""),
            None,
        )
        .await;
    assert_eq!(code, 2);
    let (code, _, _) = fx
        .run(
            "--apply",
            Some(migrator.clone()),
            "other-host",
            &db,
            Some(""),
            None,
        )
        .await;
    assert_eq!(code, 2);
    // Production-like and empty pins refuse before any DDL.
    let (code, _, err) = fx
        .run(
            "--apply",
            Some(migrator.clone()),
            "ep-prod-fixture.us-east-2.aws.neon.tech",
            &db,
            Some(""),
            None,
        )
        .await;
    assert_eq!(code, 2, "{err}");
    // Pooler endpoints refuse before any DDL.
    let (code, _, err) = fx
        .run(
            "--apply",
            Some(migrator.clone()),
            "ep-test-pooler.us-east-2.aws.neon.tech",
            &db,
            Some(""),
            None,
        )
        .await;
    assert_eq!(code, 2, "{err}");
    let (code, _, _) = fx
        .run("--apply", Some(migrator.clone()), "", &db, Some(""), None)
        .await;
    assert_eq!(code, 2);
    let (code, _, _) = fx
        .run("--apply", Some(migrator.clone()), &host, "", Some(""), None)
        .await;
    assert_eq!(code, 2);
    // Apply without the reviewed pending list refuses before any DDL.
    let (code, _, err) = fx
        .run("--apply", Some(migrator.clone()), &host, &db, None, None)
        .await;
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("expected_pending"));
    // Apply without the plan-bound hash and run id refuses before any DDL.
    let (code, _, err) = fx
        .run(
            "--apply",
            Some(migrator.clone()),
            &host,
            &db,
            Some(""),
            None,
        )
        .await;
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("plan_manifest_sha256"));
    // A login that cannot SET ROLE two_bot_migrator is refused.
    let outsider = fx.url(&fx.outsider, &fx.database);
    let (code, _, err) = fx
        .run("--apply", Some(outsider), &host, &db, Some(""), None)
        .await;
    assert_eq!(code, 2, "{err}");
    let ledger: bool =
        sqlx::query_scalar("SELECT to_regclass('public._sqlx_migrations') IS NOT NULL")
            .fetch_one(&mut c)
            .await?;
    assert!(!ledger, "refusals must not create the ledger");

    // Seed the exact 29-version staging state as the member login.
    seed_staging_state(&fx).await?;
    let (rows, max, failed): (i64, i64, i64) = sqlx::query_as(
        "SELECT count(*), coalesce(max(version), 0), \
         count(*) FILTER (WHERE NOT success) FROM public._sqlx_migrations",
    )
    .fetch_one(&mut c)
    .await?;
    assert_eq!((rows, max, failed), (29, 390, 0));
    let owner: String = sqlx::query_scalar(
        "SELECT pg_get_userbyid(relowner)::text FROM pg_class \
         WHERE oid = to_regclass('public._sqlx_migrations')",
    )
    .fetch_one(&mut c)
    .await?;
    assert_eq!(owner, "two_bot_migrator");

    // Plan is read-only and prints the computed pending list with its hash.
    let (code, out, err) = fx
        .run("--plan", Some(member.clone()), &host, &db, None, None)
        .await;
    assert_eq!(code, 0, "{err}");
    let m = manifest(&out);
    assert_eq!(m["applied_count"], 0);
    let plan_hash = m["plan_manifest_sha256"]
        .as_str()
        .expect("plan hash")
        .to_owned();
    assert_eq!(
        plan_hash.len(),
        64,
        "plan hash must be a SHA-256 hex digest"
    );
    let seed_set: HashSet<i64> = SEED_VERSIONS.into_iter().collect();
    let expected: Vec<i64> = two_bot_cutover::staging_migrate::MIGRATOR
        .iter()
        .filter(|mm| !seed_set.contains(&mm.version))
        .map(|mm| mm.version)
        .collect();
    assert_eq!(pending_list(&m["pending_before"]), expected);
    assert_eq!(m["ledger_before"].as_array().unwrap().len(), 29);
    // Pending spans both sides of the ledger max: versions below 390 were
    // never applied, yet SQLx must still pick them up.
    assert!(expected.contains(&201), "below-max gap must be pending");
    assert!(expected.contains(&410), "above-max version must be pending");
    let expected_csv = expected
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let ledger: i64 = sqlx::query_scalar("SELECT count(*) FROM public._sqlx_migrations")
        .fetch_one(&mut c)
        .await?;
    assert_eq!(ledger, 29, "plan must not change the ledger");

    // A wrong pending list refuses before any DDL.
    let (code, _, err) = fx
        .run(
            "--apply",
            Some(member.clone()),
            &host,
            &db,
            Some("201"),
            None,
        )
        .await;
    assert_eq!(code, 2, "{err}");
    let ledger: i64 = sqlx::query_scalar("SELECT count(*) FROM public._sqlx_migrations")
        .fetch_one(&mut c)
        .await?;
    assert_eq!(ledger, 29, "plan-bound refusal must not change the ledger");

    // A mismatched plan hash refuses before any DDL, even with the right list.
    let mut bad_hash = plan_hash.clone();
    bad_hash.replace_range(..2, if &plan_hash[..2] == "00" { "ff" } else { "00" });
    let (code, _, err) = fx
        .run(
            "--apply",
            Some(member.clone()),
            &host,
            &db,
            Some(&expected_csv),
            Some((&bad_hash, "424242")),
        )
        .await;
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("plan_manifest_sha256"));
    let ledger: i64 = sqlx::query_scalar("SELECT count(*) FROM public._sqlx_migrations")
        .fetch_one(&mut c)
        .await?;
    assert_eq!(ledger, 29, "hash refusal must not change the ledger");

    // Apply the reviewed list as the member login, bound to the plan hash.
    let (code, out, err) = fx
        .run(
            "--apply",
            Some(member.clone()),
            &host,
            &db,
            Some(&expected_csv),
            Some((&plan_hash, "424242")),
        )
        .await;
    assert_eq!(code, 0, "{err}");
    let m = manifest(&out);
    assert_eq!(m["applied_count"], expected.len() as u64);
    assert!(m["role_verified_connections"].as_u64().unwrap() >= 1);
    assert_eq!(m["ledger_after"].as_array().unwrap().len() as u64, total);
    assert!(!out.contains("postgres://"), "manifest must not echo URLs");
    let (rows, failed): (i64, i64) = sqlx::query_as(
        "SELECT count(*), count(*) FILTER (WHERE NOT success) \
         FROM public._sqlx_migrations",
    )
    .fetch_one(&mut c)
    .await?;
    assert_eq!((rows, failed), (total as i64, 0));
    // Below-max gaps are filled alongside above-max versions: the ledger max
    // never limited what SQLx applied.
    for version in [201, 228, 312, 321, 352, 370, 410, 411] {
        let present: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM public._sqlx_migrations WHERE version = $1 AND success)",
        )
        .bind(version)
        .fetch_one(&mut c)
        .await?;
        assert!(present, "version {version} must be applied");
    }
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

    // The reviewed list is now stale, so it refuses; an empty expectation
    // applies nothing. The no-op apply still needs a fresh plan hash, so
    // re-plan first and bind to the new empty manifest.
    let (code, _, _) = fx
        .run(
            "--apply",
            Some(member.clone()),
            &host,
            &db,
            Some(&expected_csv),
            Some((&plan_hash, "424242")),
        )
        .await;
    assert_eq!(code, 2);
    let (code, out, err) = fx
        .run("--plan", Some(member.clone()), &host, &db, None, None)
        .await;
    assert_eq!(code, 0, "{err}");
    let empty_hash = manifest(&out)["plan_manifest_sha256"]
        .as_str()
        .expect("empty plan hash")
        .to_owned();
    let (code, out, err) = fx
        .run(
            "--apply",
            Some(member.clone()),
            &host,
            &db,
            Some(""),
            Some((&empty_hash, "424243")),
        )
        .await;
    assert_eq!(code, 0, "{err}");
    assert_eq!(manifest(&out)["applied_count"], 0);

    // SHA-384 drift refuses. The binding inputs must be valid-format so the
    // run reaches reconcile (which fails on drift) instead of refusing on the
    // missing binding first.
    sqlx::query("UPDATE public._sqlx_migrations SET checksum = decode(repeat('00', 48), 'hex') WHERE version = (SELECT min(version) FROM public._sqlx_migrations)")
        .execute(&mut c)
        .await?;
    let (code, _, err) = fx
        .run(
            "--apply",
            Some(member.clone()),
            &host,
            &db,
            Some(""),
            Some((&empty_hash, "424244")),
        )
        .await;
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("drift"));
    // Restore the checksum from the source, then an incomplete row refuses.
    let (code, _, _) = fx
        .run("--plan", Some(member.clone()), &host, &db, None, None)
        .await;
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
    // Same valid-format binding here: the run must reach reconcile (which
    // fails on the incomplete row) instead of refusing on the binding first.
    let (code, _, err) = fx
        .run(
            "--apply",
            Some(member),
            &host,
            &db,
            Some(""),
            Some((&empty_hash, "424245")),
        )
        .await;
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("incomplete"));

    c.close().await?;
    fx.finish().await
}
