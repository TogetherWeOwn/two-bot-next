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
//! version regardless of the ledger max. Plan runs as a separate read-only
//! login through its own `TWO_BOT_STAGING_PLAN_DATABASE_URL` binding and
//! `SET ROLE two_bot_migrator_ro`, proving the plan path cannot DDL even
//! when pointed at the same database.

use std::{
    collections::HashSet,
    sync::atomic::{AtomicU64, Ordering},
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
        || (github_actions
            && matches!(
                url,
                "postgres://agent_test@127.0.0.1:5432/agent_test"
                    | "postgres://agent_test:@agent-testdb:5432/two_bot_test_ci"
            ))
}

#[cfg(test)]
mod url_guard_tests {
    use super::url_allowed;

    #[test]
    fn accepts_ci_coverage_database() {
        assert!(url_allowed(
            "postgres://agent_test:@agent-testdb:5432/two_bot_test_ci",
            true
        ));
        assert!(!url_allowed(
            "postgres://agent_test:@agent-testdb:5432/two_bot_test_ci",
            false
        ));
        assert!(!url_allowed(
            "postgres://agent_test@other-host:5432/two_bot_test_ci",
            true
        ));
    }
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

/// Process-wide fixture sequence. `SystemTime` nanos can repeat across threads
/// (concurrent callers read the same tick), so parallel fixtures must not rely
/// on the clock alone for unique database/role names: two fixtures sharing one
/// name fail at `CREATE DATABASE` with Postgres 23505. Concurrency is kept;
/// each fixture still gets its own database and roles.
static FIXTURE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Unique tag shared by one fixture's database and roles (and its plan-manifest
/// snapshot): pid plus clock nanos plus a process-wide sequence number.
fn fixture_tag() -> String {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = FIXTURE_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{}_{}_{}", std::process::id(), nonce, seq)
}

/// The reviewed expectations a runner invocation binds: the pending lists
/// for both chains and, for apply, the producing plan's binding. Grouped so
/// the `Fixture::run*` helpers stay within the argument limit without a
/// clippy waiver.
#[derive(Default)]
struct RunExpect<'a> {
    expected_pending: Option<&'a str>,
    expected_store_pending: Option<&'a str>,
    plan_binding: Option<(&'a str, &'a str, &'a str, &'a str)>,
}

impl<'a> RunExpect<'a> {
    fn pending(expected_pending: Option<&'a str>, expected_store_pending: Option<&'a str>) -> Self {
        Self {
            expected_pending,
            expected_store_pending,
            plan_binding: None,
        }
    }

    fn bound(
        expected_pending: Option<&'a str>,
        expected_store_pending: Option<&'a str>,
        plan_binding: (&'a str, &'a str, &'a str, &'a str),
    ) -> Self {
        Self {
            expected_pending,
            expected_store_pending,
            plan_binding: Some(plan_binding),
        }
    }
}

struct Fixture {
    admin: PgConnection,
    host: String,
    database: String,
    outsider: String,
    mlogin: String,
    rologin: String,
    /// Holds both groups: connects and `SET ROLE`s to the RO group, but is not RO only.
    bothlogin: String,
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
        let tag = fixture_tag();
        let database = format!("two_bot_staging_fx_{tag}");
        let outsider = format!("stg_fx_outsider_{tag}");
        let mlogin = format!("stg_fx_mlogin_{tag}");
        let rologin = format!("stg_fx_rologin_{tag}");
        let bothlogin = format!("stg_fx_bothlogin_{tag}");
        // Cluster-level groups shared with other suites: tolerate concurrent creates.
        sqlx::query(
            "DO $$ BEGIN CREATE ROLE two_bot_migrator NOLOGIN; \
             EXCEPTION WHEN duplicate_object OR unique_violation THEN NULL; END $$",
        )
        .execute(&mut admin)
        .await?;
        sqlx::query(
            "DO $$ BEGIN CREATE ROLE two_bot_migrator_ro NOLOGIN; \
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
            // Read-only plan login: holds only the RO group, never the migrator.
            format!("CREATE ROLE {rologin} LOGIN"),
            // Holds both groups: the plan runner must refuse it even though
            // it can SET ROLE two_bot_migrator_ro.
            format!("CREATE ROLE {bothlogin} LOGIN"),
        ] {
            sqlx::query(sqlx::AssertSqlSafe(sql))
                .execute(&mut admin)
                .await?;
        }
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "GRANT CONNECT ON DATABASE {database} TO two_bot_migrator, two_bot_migrator_ro"
        )))
        .execute(&mut admin)
        .await?;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "GRANT two_bot_migrator TO {mlogin}"
        )))
        .execute(&mut admin)
        .await?;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "GRANT two_bot_migrator_ro TO {rologin}"
        )))
        .execute(&mut admin)
        .await?;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "GRANT two_bot_migrator, two_bot_migrator_ro TO {bothlogin}"
        )))
        .execute(&mut admin)
        .await?;
        // Card-named audit logins: fixed names, so concurrent suites share
        // them. The plan login holds the RO group; the apply login is bare.
        // Both prove the audit decodes `exists` true with exact memberships.
        for role in ["two_bot_migrator_ro_plan", "two_bot_migrator_apply"] {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "DO $$ BEGIN CREATE ROLE {role} NOLOGIN; \
                 EXCEPTION WHEN duplicate_object OR unique_violation THEN NULL; END $$"
            )))
            .execute(&mut admin)
            .await?;
        }
        sqlx::query(
            "DO $$ BEGIN IF NOT EXISTS (SELECT 1 FROM pg_auth_members am \
             JOIN pg_roles r ON r.oid = am.member \
             JOIN pg_roles m ON m.oid = am.roleid \
             WHERE r.rolname = 'two_bot_migrator_ro_plan' \
             AND m.rolname = 'two_bot_migrator_ro') THEN \
             GRANT two_bot_migrator_ro TO two_bot_migrator_ro_plan; END IF; END $$",
        )
        .execute(&mut admin)
        .await?;
        let mut scratch = PgConnection::connect_with(&admin_options(host, &database)).await?;
        sqlx::query("ALTER SCHEMA public OWNER TO two_bot_migrator")
            .execute(&mut scratch)
            .await?;
        // The plan path reads only the ledger and schema presence, so the
        // fixture RO login gets exactly that: schema USAGE plus ledger SELECT.
        // (Production grants the full SELECT matrix from `sql/database_roles.sql`.)
        // Mirror the production render: PUBLIC holds no CREATE on `public` (the
        // default on PostgreSQL before 15), so only the schema owner can DDL.
        sqlx::query("REVOKE CREATE ON SCHEMA public FROM PUBLIC")
            .execute(&mut scratch)
            .await?;
        sqlx::query("GRANT USAGE ON SCHEMA public TO two_bot_migrator_ro")
            .execute(&mut scratch)
            .await?;
        scratch.close().await?;
        Ok(Self {
            admin,
            host: host.to_owned(),
            database,
            outsider,
            mlogin,
            rologin,
            bothlogin,
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
        expect: RunExpect<'_>,
    ) -> (i32, String, String) {
        self.run_with_stray(mode, url, None, host, db, expect).await
    }

    /// Like [`Self::run`], but also sets `stray` as the OTHER mode's binding:
    /// proves a mode never borrows the credential reserved for the other one.
    async fn run_with_stray(
        &self,
        mode: &str,
        url: Option<String>,
        stray: Option<String>,
        host: &str,
        db: &str,
        expect: RunExpect<'_>,
    ) -> (i32, String, String) {
        let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_staging-migrate"));
        cmd.args([
            "--target",
            "staging",
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
        if let Some(list) = expect.expected_pending {
            cmd.args(["--expected-pending", list]);
        }
        if let Some(list) = expect.expected_store_pending {
            cmd.args(["--expected-store-pending", list]);
        }
        if let Some((hash, store_hash, run_id, manifest_path)) = expect.plan_binding {
            cmd.args([
                "--plan-manifest-sha256",
                hash,
                "--store-plan-manifest-sha256",
                store_hash,
                "--plan-run-id",
                run_id,
                "--plan-manifest-path",
                manifest_path,
            ]);
        }
        // Each mode reads only its own binding: the runner refuses when the
        // mode's binding is absent, even when the other mode's URL is set.
        let (binding, other) = if mode == "--plan" {
            (
                "TWO_BOT_STAGING_PLAN_DATABASE_URL",
                "TWO_BOT_STAGING_MIGRATOR_DATABASE_URL",
            )
        } else {
            (
                "TWO_BOT_STAGING_MIGRATOR_DATABASE_URL",
                "TWO_BOT_STAGING_PLAN_DATABASE_URL",
            )
        };
        for key in [
            "TWO_BOT_STAGING_PLAN_DATABASE_URL",
            "TWO_BOT_STAGING_MIGRATOR_DATABASE_URL",
            "TWO_BOT_PRODUCTION_PLAN_DATABASE_URL",
            "TWO_BOT_PRODUCTION_MIGRATOR_DATABASE_URL",
            "PGOPTIONS",
            "PGPASSFILE",
            "PGSERVICE",
        ] {
            cmd.env_remove(key);
        }
        // The runner fences its target pool with the `TWO_DATABASE_TLS`
        // policy: the disposable CI-service URLs are plaintext local hosts,
        // so pin `local-only` explicitly instead of inheriting the ambient
        // setting.
        cmd.env("TWO_DATABASE_TLS", "local-only");
        if let Some(url) = url {
            cmd.env(binding, url);
        }
        if let Some(stray) = stray {
            cmd.env(other, stray);
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

    /// A session as the read-only plan login with its group assumed, exactly
    /// as the runner's `after_connect` does for `--plan`.
    async fn ro_session(&self) -> Result<PgConnection, sqlx::Error> {
        let options = PgConnectOptions::new_without_pgpass()
            .host(&self.host)
            .port(5432)
            .username(&self.rologin)
            .password("")
            .database(&self.database)
            .ssl_mode(PgSslMode::Disable);
        let mut conn = PgConnection::connect_with(&options).await?;
        conn.execute("SET ROLE two_bot_migrator_ro").await?;
        Ok(conn)
    }

    async fn finish(mut self) -> TestResult {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP DATABASE {} WITH (FORCE)",
            self.database
        )))
        .execute(&mut self.admin)
        .await?;
        for role in [&self.outsider, &self.mlogin, &self.rologin, &self.bothlogin] {
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

/// Snapshot a plan run's stdout to a temp file, mirroring the workflow's
/// `staging-migrate-manifest.json` artifact. The caller passes the path as
/// `--plan-manifest-path` so apply proves the bound hash against the
/// producing plan's manifest. Returns the path; the OS reclaims temp files.
fn snapshot_plan_manifest(stdout: &str) -> String {
    let path = std::env::temp_dir().join(format!("staging-migrate-db-plan-{}.json", fixture_tag()));
    std::fs::write(&path, stdout).expect("plan snapshot must be writable");
    path.to_string_lossy().into_owned()
}

fn pending_list(value: &Value) -> Vec<i64> {
    value
        .as_array()
        .expect("pending list")
        .iter()
        .map(|v| v.as_i64().expect("version"))
        .collect()
}

/// Guard: fixture tags stay unique when generated from many threads at once.
/// Clock nanos alone can repeat within one tick, which used to collide parallel
/// fixtures on the same database name (`CREATE DATABASE` then fails with
/// Postgres 23505). Needs no database, so it runs outside `--ignored`.
#[test]
fn fixture_tag_is_unique_under_concurrency() {
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(16));
    let workers: Vec<_> = (0..16)
        .map(|_| {
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                (0..1000).map(|_| fixture_tag()).collect::<Vec<_>>()
            })
        })
        .collect();
    let mut seen = HashSet::new();
    let mut total = 0;
    for worker in workers {
        for tag in worker.join().expect("worker") {
            total += 1;
            assert!(seen.insert(tag), "duplicate fixture tag under concurrency");
        }
    }
    assert_eq!(total, 16_000);
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
    let ro = fx.url(&fx.rologin, &fx.database);
    let both = fx.url(&fx.bothlogin, &fx.database);
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
    let (code, _, err) = fx
        .run(
            "--apply",
            None,
            &host,
            &db,
            RunExpect::pending(Some(""), Some("")),
        )
        .await;
    assert_eq!(code, 2, "{err}");
    let (code, _, _) = fx
        .run(
            "--apply",
            Some(migrator.clone()),
            &host,
            "two_bot_staging_other",
            RunExpect::pending(Some(""), Some("")),
        )
        .await;
    assert_eq!(code, 2);
    let (code, _, _) = fx
        .run(
            "--apply",
            Some(migrator.clone()),
            "other-host",
            &db,
            RunExpect::pending(Some(""), Some("")),
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
            RunExpect::pending(Some(""), Some("")),
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
            RunExpect::pending(Some(""), Some("")),
        )
        .await;
    assert_eq!(code, 2, "{err}");
    let (code, _, _) = fx
        .run(
            "--apply",
            Some(migrator.clone()),
            "",
            &db,
            RunExpect::pending(Some(""), Some("")),
        )
        .await;
    assert_eq!(code, 2);
    let (code, _, _) = fx
        .run(
            "--apply",
            Some(migrator.clone()),
            &host,
            "",
            RunExpect::pending(Some(""), Some("")),
        )
        .await;
    assert_eq!(code, 2);
    // Apply without the reviewed pending list refuses before any DDL.
    let (code, _, err) = fx
        .run(
            "--apply",
            Some(migrator.clone()),
            &host,
            &db,
            RunExpect::default(),
        )
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
            RunExpect::pending(Some(""), Some("")),
        )
        .await;
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("plan_manifest_sha256"));
    // A login that cannot SET ROLE two_bot_migrator is refused.
    let outsider = fx.url(&fx.outsider, &fx.database);
    let (code, _, err) = fx
        .run(
            "--apply",
            Some(outsider),
            &host,
            &db,
            RunExpect::pending(Some(""), Some("")),
        )
        .await;
    assert_eq!(code, 2, "{err}");
    // "RO only" is enforced: a plan login that holds the migrator group is
    // refused, whether it also holds the RO group (it can SET ROLE to it) or
    // is a superuser (implicitly a member of every group). The refusal names
    // the role, echoes neither the URL nor the login, and emits no manifest.
    for (label, url, login) in [
        ("both groups", both.clone(), fx.bothlogin.clone()),
        ("superuser", migrator.clone(), "agent_test".to_owned()),
    ] {
        let (code, out, err) = fx
            .run(
                "--plan",
                Some(url.clone()),
                &host,
                &db,
                RunExpect::default(),
            )
            .await;
        assert_eq!(code, 2, "{label}: {err}");
        assert!(err.contains("two_bot_migrator group"), "{label}: {err}");
        assert!(err.contains("two_bot_migrator_ro"), "{label}: {err}");
        assert!(
            !err.contains(&url),
            "{label}: refusal must not echo the URL"
        );
        assert!(
            !err.contains(&format!("{login}@")),
            "{label}: refusal must not echo the login"
        );
        assert!(out.is_empty(), "{label}: refusal must not emit a manifest");
    }
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

    // The plan path reads only the ledger, so the fixture RO group gets exactly
    // that (production grants the full SELECT matrix from `sql/database_roles.sql`).
    sqlx::query("GRANT SELECT ON public._sqlx_migrations TO two_bot_migrator_ro")
        .execute(&mut c)
        .await?;

    // The RO login is physically incapable of DDL or ledger writes: this is
    // the database-enforced boundary, not a runner convention.
    let mut ro_conn = fx.ro_session().await?;
    let readable: i64 = sqlx::query_scalar("SELECT count(*) FROM public._sqlx_migrations")
        .fetch_one(&mut ro_conn)
        .await?;
    assert_eq!(readable, 29, "RO group must read the ledger");
    for denied in [
        "CREATE TABLE public.ro_probe (id int)",
        "ALTER TABLE public._sqlx_migrations ADD COLUMN ro_probe int",
        "DROP TABLE public._sqlx_migrations",
        "DELETE FROM public._sqlx_migrations",
        "INSERT INTO public._sqlx_migrations \
         (version, description, success, checksum, execution_time) \
         VALUES (999999, 'ro probe', true, '\\x00', 0)",
    ] {
        assert!(
            sqlx::query(denied).execute(&mut ro_conn).await.is_err(),
            "RO role must be refused: {denied}"
        );
    }
    ro_conn.close().await?;
    let ledger: i64 = sqlx::query_scalar("SELECT count(*) FROM public._sqlx_migrations")
        .fetch_one(&mut c)
        .await?;
    assert_eq!(ledger, 29, "RO probes must not change the ledger");

    // Each mode reads only its own binding. A plan run with only the migrator
    // binding set refuses on the absent RO binding, before connecting.
    let (code, out, err) = fx
        .run_with_stray(
            "--plan",
            None,
            Some(migrator.clone()),
            &host,
            &db,
            RunExpect::default(),
        )
        .await;
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("TWO_BOT_STAGING_PLAN_DATABASE_URL"), "{err}");
    assert!(out.is_empty(), "refusal must not emit a manifest");
    // The migrator-group login cannot be used as the plan credential either:
    // it does not hold the RO group, so SET ROLE two_bot_migrator_ro refuses.
    let (code, _, err) = fx
        .run(
            "--plan",
            Some(member.clone()),
            &host,
            &db,
            RunExpect::default(),
        )
        .await;
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("SET ROLE two_bot_migrator_ro"), "{err}");
    // And the RO login cannot be used for apply: SET ROLE two_bot_migrator
    // refuses before any DDL, even with a well-formed plan binding.
    let (code, _, err) = fx
        .run(
            "--apply",
            Some(ro.clone()),
            &host,
            &db,
            RunExpect::bound(
                Some(""),
                Some(""),
                (
                    &"a".repeat(64),
                    &"a".repeat(64),
                    "424240",
                    "producing-plan/staging-migrate-manifest.json",
                ),
            ),
        )
        .await;
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("SET ROLE two_bot_migrator"), "{err}");
    let ledger: i64 = sqlx::query_scalar("SELECT count(*) FROM public._sqlx_migrations")
        .fetch_one(&mut c)
        .await?;
    assert_eq!(
        ledger, 29,
        "cross-credential refusals must not change the ledger"
    );

    // Plan is read-only and prints the computed pending list with its hash.
    let (code, out, err) = fx
        .run("--plan", Some(ro.clone()), &host, &db, RunExpect::default())
        .await;
    assert_eq!(code, 0, "{err}");
    let m = manifest(&out);
    assert_eq!(m["role"], "two_bot_migrator_ro");
    assert!(m["role_verified_connections"].as_u64().unwrap() >= 1);
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
    // The store chain plans alongside with its own ledger, pending list and
    // binding hash; the fixture never applied it, so every store version is
    // pending and nothing is applied by a read-only plan.
    let store_hash = m["store_plan_manifest_sha256"]
        .as_str()
        .expect("store plan hash")
        .to_owned();
    assert_eq!(
        store_hash.len(),
        64,
        "store plan hash must be a SHA-256 hex digest"
    );
    assert_ne!(store_hash, plan_hash, "each chain binds its own hash");
    assert_eq!(m["applied_store_count"], 0);
    assert_eq!(
        m["ledger_store_before"], m["ledger_store_after"],
        "plan must not change the store ledger"
    );
    assert!(
        m["ledger_store_before"].as_array().unwrap().is_empty(),
        "fixture never applied the store chain"
    );
    // Snapshot the plan stdout as the producing run's artifact: every apply
    // below proves its bound hash against this manifest.
    let plan_artifact = snapshot_plan_manifest(&out);
    let seed_set: HashSet<i64> = SEED_VERSIONS.into_iter().collect();
    let expected: Vec<i64> = two_bot_cutover::staging_migrate::MIGRATOR
        .iter()
        .filter(|mm| !seed_set.contains(&mm.version))
        .map(|mm| mm.version)
        .collect();
    assert_eq!(pending_list(&m["pending_before"]), expected);
    assert_eq!(m["ledger_before"].as_array().unwrap().len(), 29);
    // The plan-only readout ran under the read-only role: it reports the ledger
    // owner, counts and verifier findings, and never an error.
    let audit = &m["audit"];
    assert!(
        audit["error"].is_null(),
        "readout failed: {}",
        audit["error"]
    );
    assert!(audit["ledger_owner"].as_str().is_some());
    assert_eq!(audit["ledger_counts"]["successful_rows"], 29);
    assert_eq!(audit["ledger_counts"]["failed_rows"], 0);
    // Every card login reports an entry, even the apply login that holds
    // nothing: `exists` separates "missing login" from "no memberships".
    let entries = audit["memberships"].as_array().unwrap();
    assert_eq!(entries.len(), 4);
    let entry = |login: &str| entries.iter().find(|e| e["login"] == login).unwrap();
    let plan_entry = entry("two_bot_migrator_ro_plan");
    assert!(plan_entry["exists"].as_bool().unwrap());
    let plan_groups = plan_entry["member_of"].as_array().unwrap();
    assert!(plan_groups.iter().any(|m| m == "two_bot_migrator_ro"));
    assert!(plan_entry["member_of_ro"].as_bool().unwrap());
    assert!(!plan_entry["member_of_migrator"].as_bool().unwrap());
    let apply_entry = entry("two_bot_migrator_apply");
    assert!(apply_entry["exists"].as_bool().unwrap());
    assert!(apply_entry["member_of"].as_array().unwrap().is_empty());
    assert!(!apply_entry["member_of_ro"].as_bool().unwrap());
    assert!(!apply_entry["member_of_migrator"].as_bool().unwrap());
    assert!(entry("two_bot_migrator")["exists"].as_bool().unwrap());
    assert!(entry("two_bot_migrator_ro")["exists"].as_bool().unwrap());
    assert!(audit["verify_findings"].is_array());
    assert_eq!(audit["matrix_sha256"].as_str().map(str::len), Some(64));
    assert_eq!(audit["verify_sha256"].as_str().map(str::len), Some(64));
    // The plan manifest carries connection usage for the 80% warning: the
    // login ceiling is max minus reserved, and the ratio is against it.
    let usage = &audit["connection_usage"];
    let used = usage["used"].as_i64().expect("connection used");
    let max = usage["max_connections"].as_i64().expect("max connections");
    let reserved = usage["reserved_connections"]
        .as_i64()
        .expect("reserved connections");
    let effective = usage["effective_max_connections"]
        .as_i64()
        .expect("effective ceiling");
    assert!(used >= 0, "connection usage must be present");
    assert!(max > 0, "max_connections must be positive");
    assert_eq!(
        effective,
        max - reserved,
        "login ceiling is max minus reserved"
    );
    let pct = usage["pct_full"].as_f64().expect("pct_full");
    let expected_pct = used as f64 / effective.max(1) as f64 * 100.0;
    assert!(
        (pct - expected_pct).abs() < 1e-9,
        "pct_full is against the login ceiling"
    );
    // Pending spans both sides of the ledger max: versions below 390 were
    // never applied, yet SQLx must still pick them up.
    assert!(expected.contains(&201), "below-max gap must be pending");
    assert!(expected.contains(&410), "above-max version must be pending");
    let expected_csv = expected
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(",");
    // The store pending list comes from the same reviewed plan manifest, so
    // the apply below binds both chains to one producing run.
    let store_expected = pending_list(&m["pending_store_before"]);
    assert!(
        !store_expected.is_empty(),
        "store chain must be pending on a fresh fixture"
    );
    let store_csv = store_expected
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
            RunExpect::pending(Some("201"), None),
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
            RunExpect::bound(
                Some(&expected_csv),
                Some(&store_csv),
                (&bad_hash, &store_hash, "424242", &plan_artifact),
            ),
        )
        .await;
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("plan_manifest_sha256"));
    let ledger: i64 = sqlx::query_scalar("SELECT count(*) FROM public._sqlx_migrations")
        .fetch_one(&mut c)
        .await?;
    assert_eq!(ledger, 29, "hash refusal must not change the ledger");

    // The right hash with no producing manifest refuses before any DDL: the
    // hash is exact but its provenance against the named plan run is
    // unprovable.
    let (code, _, err) = fx
        .run(
            "--apply",
            Some(member.clone()),
            &host,
            &db,
            RunExpect::bound(
                Some(&expected_csv),
                Some(&store_csv),
                (
                    &plan_hash,
                    &store_hash,
                    "424242",
                    "/tmp/staging-migrate-db-no-such-manifest.json",
                ),
            ),
        )
        .await;
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("plan_run_id"), "{err}");
    let ledger: i64 = sqlx::query_scalar("SELECT count(*) FROM public._sqlx_migrations")
        .fetch_one(&mut c)
        .await?;
    assert_eq!(ledger, 29, "provenance refusal must not change the ledger");

    // Apply the reviewed list as the member login, bound to the plan hash.
    // Both chains apply: the cutover list plus the full store chain, each
    // under its own ledger.
    let (code, out, err) = fx
        .run(
            "--apply",
            Some(member.clone()),
            &host,
            &db,
            RunExpect::bound(
                Some(&expected_csv),
                Some(&store_csv),
                (&plan_hash, &store_hash, "424242", &plan_artifact),
            ),
        )
        .await;
    assert_eq!(code, 0, "{err}");
    let m = manifest(&out);
    assert_eq!(m["applied_count"], expected.len() as u64);
    assert_eq!(
        m["applied_store_count"],
        store_expected.len() as u64,
        "apply must run the store chain too"
    );
    assert_eq!(
        m["ledger_store_after"].as_array().unwrap().len(),
        store_expected.len()
    );
    assert_eq!(
        m["pending_store_before"],
        serde_json::json!(store_expected),
        "store pending must be the reviewed list"
    );
    assert_eq!(m["role"], "two_bot_migrator", "apply path is unchanged");
    assert_eq!(m["plan_provenance_verified"], true);
    assert!(m["audit"].is_null(), "apply emits no plan readout");
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
    // The store chain landed in its own ledger, owned by the migrator, with
    // every version successful.
    let (store_rows, store_failed): (i64, i64) = sqlx::query_as(
        "SELECT count(*), count(*) FILTER (WHERE NOT success) \
         FROM public._two_bot_migrations",
    )
    .fetch_one(&mut c)
    .await?;
    assert_eq!(
        (store_rows, store_failed),
        (store_expected.len() as i64, 0),
        "store ledger must hold every store version"
    );
    let store_owner: String = sqlx::query_scalar(
        "SELECT pg_get_userbyid(relowner)::text FROM pg_class \
         WHERE oid = to_regclass('public._two_bot_migrations')",
    )
    .fetch_one(&mut c)
    .await?;
    assert_eq!(store_owner, "two_bot_migrator");
    // Later re-plans read the store ledger as the RO login, so the fixture
    // grants exactly that read (production grants it from the role matrix).
    sqlx::query("GRANT SELECT ON public._two_bot_migrations TO two_bot_migrator_ro")
        .execute(&mut c)
        .await?;
    let mut ro_conn = fx.ro_session().await?;
    let store_readable: i64 = sqlx::query_scalar("SELECT count(*) FROM public._two_bot_migrations")
        .fetch_one(&mut ro_conn)
        .await?;
    assert_eq!(
        store_readable,
        store_expected.len() as i64,
        "RO group must read the store ledger"
    );
    ro_conn.close().await?;
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
            RunExpect::bound(
                Some(&expected_csv),
                Some(&store_csv),
                (&plan_hash, &store_hash, "424242", &plan_artifact),
            ),
        )
        .await;
    assert_eq!(code, 2);
    let (code, out, err) = fx
        .run("--plan", Some(ro.clone()), &host, &db, RunExpect::default())
        .await;
    assert_eq!(code, 0, "{err}");
    let empty_hash = manifest(&out)["plan_manifest_sha256"]
        .as_str()
        .expect("empty plan hash")
        .to_owned();
    let empty_store_hash = manifest(&out)["store_plan_manifest_sha256"]
        .as_str()
        .expect("empty store plan hash")
        .to_owned();
    assert!(
        manifest(&out)["pending_store_before"]
            .as_array()
            .unwrap()
            .is_empty(),
        "store chain must be fully applied"
    );
    let empty_artifact = snapshot_plan_manifest(&out);
    let (code, out, err) = fx
        .run(
            "--apply",
            Some(member.clone()),
            &host,
            &db,
            RunExpect::bound(
                Some(""),
                Some(""),
                (&empty_hash, &empty_store_hash, "424243", &empty_artifact),
            ),
        )
        .await;
    assert_eq!(code, 0, "{err}");
    assert_eq!(manifest(&out)["applied_count"], 0);
    assert_eq!(manifest(&out)["applied_store_count"], 0);

    // SHA-384 drift refuses. The binding inputs must be valid-format so the
    // run reaches reconcile (which fails on drift) instead of refusing on the
    // missing binding first. The provenance anchor must verify too, so the
    // empty plan's artifact travels with the binding.
    sqlx::query("UPDATE public._sqlx_migrations SET checksum = decode(repeat('00', 48), 'hex') WHERE version = (SELECT min(version) FROM public._sqlx_migrations)")
        .execute(&mut c)
        .await?;
    let (code, _, err) = fx
        .run(
            "--apply",
            Some(member.clone()),
            &host,
            &db,
            RunExpect::bound(
                Some(""),
                Some(""),
                (&empty_hash, &empty_store_hash, "424244", &empty_artifact),
            ),
        )
        .await;
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("drift"));
    // Restore the checksum from the source, then an incomplete row refuses.
    let (code, _, _) = fx
        .run("--plan", Some(ro.clone()), &host, &db, RunExpect::default())
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
            RunExpect::bound(
                Some(""),
                Some(""),
                (&empty_hash, &empty_store_hash, "424245", &empty_artifact),
            ),
        )
        .await;
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("incomplete"));

    c.close().await?;
    fx.finish().await
}

/// Marker the runner records when the ledger cannot be read after a run.
const LEDGER_UNKNOWN: &str = "unknown: ledger unreadable";

struct PostRunFault {
    code: i32,
    stdout: String,
    stderr: String,
    present: Vec<i64>,
    pending: Vec<i64>,
}

/// Real apply whose post-run ledger read is denied by a committed migration.
///
/// The first pending migration commits a grant change that revokes table-level
/// SELECT from the migrator, keeping only the columns SQLx itself reads. The
/// runner's read of `description` is then denied after the run. With
/// `fail_second`, the second pending migration fails on its ledger insert, so
/// the run fails while the read is still broken.
async fn apply_with_post_run_read_fault(
    revoke_read: bool,
    fail_second: bool,
) -> Result<PostRunFault, Box<dyn std::error::Error + Send + Sync>> {
    let fx = Fixture::new().await?;
    seed_staging_state(&fx).await?;
    let mut c = fx.scratch().await?;
    sqlx::query("GRANT SELECT ON public._sqlx_migrations TO two_bot_migrator_ro")
        .execute(&mut c)
        .await?;
    c.close().await?;
    let (host, db) = (fx.host.clone(), fx.database.clone());
    let member = fx.url(&fx.mlogin, &fx.database);
    let ro = fx.url(&fx.rologin, &fx.database);

    let (code, out, err) = fx
        .run("--plan", Some(ro), &host, &db, RunExpect::default())
        .await;
    assert_eq!(code, 0, "{err}");
    let plan = manifest(&out);
    let plan_hash = plan["plan_manifest_sha256"]
        .as_str()
        .expect("plan hash")
        .to_owned();
    let store_hash = plan["store_plan_manifest_sha256"]
        .as_str()
        .expect("store plan hash")
        .to_owned();
    let plan_artifact = snapshot_plan_manifest(&out);
    let pending = pending_list(&plan["pending_before"]);
    assert!(pending.len() >= 2, "need two pending migrations to fault");
    let expected_csv = pending
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(",");
    // The store chain is fully pending on this fresh fixture; bind it too so
    // the apply reaches the cutover fault instead of refusing on the store
    // binding first.
    let store_pending = pending_list(&plan["pending_store_before"]);
    assert!(
        !store_pending.is_empty(),
        "store chain must be pending on a fresh fixture"
    );
    let store_csv = store_pending
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(",");

    let revoke_at = revoke_read.then_some(pending[0]);
    let raise_at = fail_second.then_some(pending[1]);
    install_post_run_fault(&fx, revoke_at, raise_at).await?;

    let (code, stdout, stderr) = fx
        .run(
            "--apply",
            Some(member),
            &host,
            &db,
            RunExpect::bound(
                Some(&expected_csv),
                Some(&store_csv),
                (&plan_hash, &store_hash, "424242", &plan_artifact),
            ),
        )
        .await;
    let mut c = fx.scratch().await?;
    let present: Vec<i64> = sqlx::query_scalar(
        "SELECT version FROM public._sqlx_migrations WHERE success ORDER BY version",
    )
    .fetch_all(&mut c)
    .await?;
    c.close().await?;
    fx.finish().await?;
    Ok(PostRunFault {
        code,
        stdout,
        stderr,
        present,
        pending,
    })
}

async fn install_post_run_fault(
    fx: &Fixture,
    revoke_at: Option<i64>,
    raise_at: Option<i64>,
) -> TestResult {
    let mut body = String::new();
    if let Some(version) = revoke_at {
        body.push_str(&format!(
            "IF NEW.version = {version} THEN \
             REVOKE SELECT ON public._sqlx_migrations FROM two_bot_migrator; \
             GRANT SELECT (version, success, checksum) ON public._sqlx_migrations \
             TO two_bot_migrator; END IF;"
        ));
    }
    if let Some(version) = raise_at {
        body.push_str(&format!(
            "IF NEW.version = {version} THEN \
             RAISE EXCEPTION 'injected migration failure'; END IF;"
        ));
    }
    let mut c = fx.scratch().await?;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "CREATE FUNCTION public.stg_fx_post_run_fault() RETURNS trigger \
         LANGUAGE plpgsql AS $$ BEGIN {body} RETURN NEW; END $$"
    )))
    .execute(&mut c)
    .await?;
    sqlx::query(
        "CREATE TRIGGER stg_fx_post_run_fault BEFORE INSERT ON public._sqlx_migrations \
         FOR EACH ROW EXECUTE FUNCTION public.stg_fx_post_run_fault()",
    )
    .execute(&mut c)
    .await?;
    c.close().await?;
    Ok(())
}

/// Fault: the post-failure read is denied. The evidence must say unknown, never
/// an empty ledger, and the original migration error must still be returned.
#[tokio::test]
#[ignore = "requires agent-testdb or the CI service"]
async fn post_failure_unreadable_ledger_is_unknown_not_empty() -> TestResult {
    let run = apply_with_post_run_read_fault(true, true).await?;
    assert_eq!(run.code, 1, "{}", run.stderr);
    assert!(
        run.stderr.contains("migration execution failed"),
        "{}",
        run.stderr
    );
    let evidence = manifest(&run.stdout);
    assert_eq!(
        evidence["ledger_after_failure"], LEDGER_UNKNOWN,
        "{}",
        run.stdout
    );
    let error = evidence["error"].as_str().expect("original error");
    assert!(error.contains("injected migration failure"), "{error}");
    // Only the first pending migration committed; the failed one left no row.
    let mut expected = SEED_VERSIONS.to_vec();
    expected.push(run.pending[0]);
    expected.sort_unstable();
    assert_eq!(run.present, expected);
    Ok(())
}

/// Control: a readable ledger after a failed apply keeps the array output.
#[tokio::test]
#[ignore = "requires agent-testdb or the CI service"]
async fn post_failure_readable_ledger_keeps_array_output() -> TestResult {
    let run = apply_with_post_run_read_fault(false, true).await?;
    assert_eq!(run.code, 1, "{}", run.stderr);
    let evidence = manifest(&run.stdout);
    let ledger = evidence["ledger_after_failure"]
        .as_array()
        .expect("readable ledger stays an array");
    assert_eq!(ledger.len(), SEED_VERSIONS.len() + 1);
    assert!(ledger.iter().any(|row| row["version"] == run.pending[0]));
    let error = evidence["error"].as_str().expect("original error");
    assert!(error.contains("injected migration failure"), "{error}");
    Ok(())
}

/// Fault after a successful apply: refuse as unreadable, not as an incomplete
/// ledger that reports an empty list.
#[tokio::test]
#[ignore = "requires agent-testdb or the CI service"]
async fn post_apply_unreadable_ledger_is_unknown_not_empty() -> TestResult {
    let run = apply_with_post_run_read_fault(true, false).await?;
    assert_eq!(run.code, 1, "{}", run.stderr);
    assert!(
        run.stderr.contains("ledger unreadable after apply"),
        "{}",
        run.stderr
    );
    assert!(!run.stderr.contains("ledger incomplete"), "{}", run.stderr);
    assert_eq!(manifest(&run.stdout)["ledger_after"], LEDGER_UNKNOWN);
    let total = two_bot_cutover::staging_migrate::MIGRATOR.iter().count();
    assert_eq!(run.present.len(), total, "every pending migration applied");
    Ok(())
}

/// Fault: the store chain fails after the cutover chain already committed.
/// A raising trigger on the empty store ledger fails the first store insert,
/// so the apply exits 1 with the store failure key plus the committed
/// cutover evidence. Removing the `with_cutover_evidence` wiring or renaming
/// the store key back fails this test.
#[tokio::test]
#[ignore = "requires agent-testdb or the CI service"]
async fn store_failure_keeps_committed_cutover_evidence() -> TestResult {
    let fx = Fixture::new().await?;
    seed_staging_state(&fx).await?;
    // Create the store ledger empty with the migrator as owner, without
    // running any store DDL: `ensure_migrations_table` runs even for an
    // empty migrator, so the cutover apply below still sees a pristine
    // schema in its normal first-chain order.
    {
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
        let mut empty = Migrator::with_migrations(Vec::new());
        empty.dangerous_set_table_name(two_bot_store::TABLE_NAME);
        empty.run(&pool).await?;
        pool.close().await;
    }
    let mut c = fx.scratch().await?;
    sqlx::query(
        "CREATE FUNCTION public.stg_fx_store_fail() RETURNS trigger LANGUAGE plpgsql \
         AS $$ BEGIN RAISE EXCEPTION 'injected store migration failure'; END $$",
    )
    .execute(&mut c)
    .await?;
    sqlx::query(
        "CREATE TRIGGER stg_fx_store_fail BEFORE INSERT ON public._two_bot_migrations \
         FOR EACH ROW EXECUTE FUNCTION public.stg_fx_store_fail()",
    )
    .execute(&mut c)
    .await?;
    sqlx::query("GRANT SELECT ON public._sqlx_migrations TO two_bot_migrator_ro")
        .execute(&mut c)
        .await?;
    sqlx::query("GRANT SELECT ON public._two_bot_migrations TO two_bot_migrator_ro")
        .execute(&mut c)
        .await?;
    c.close().await?;
    let (host, db) = (fx.host.clone(), fx.database.clone());
    let member = fx.url(&fx.mlogin, &fx.database);
    let ro = fx.url(&fx.rologin, &fx.database);

    let (code, out, err) = fx
        .run("--plan", Some(ro), &host, &db, RunExpect::default())
        .await;
    assert_eq!(code, 0, "{err}");
    let plan = manifest(&out);
    let plan_hash = plan["plan_manifest_sha256"]
        .as_str()
        .expect("plan hash")
        .to_owned();
    let store_hash = plan["store_plan_manifest_sha256"]
        .as_str()
        .expect("store plan hash")
        .to_owned();
    let plan_artifact = snapshot_plan_manifest(&out);
    let pending = pending_list(&plan["pending_before"]);
    assert!(!pending.is_empty(), "cutover chain must be pending");
    let expected_csv = pending
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let store_pending = pending_list(&plan["pending_store_before"]);
    assert!(!store_pending.is_empty(), "store chain must be pending");
    let store_csv = store_pending
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(",");

    let (code, stdout, stderr) = fx
        .run(
            "--apply",
            Some(member),
            &host,
            &db,
            RunExpect::bound(
                Some(&expected_csv),
                Some(&store_csv),
                (&plan_hash, &store_hash, "424242", &plan_artifact),
            ),
        )
        .await;
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("migration execution failed"), "{stderr}");
    let evidence = manifest(&stdout);
    // The store chain's own failure key (success-manifest family), never the
    // old `store_ledger_after` name and never the cutover key.
    assert!(
        evidence["ledger_store_after_failure"].is_array(),
        "store failure key must be an array: {stdout}"
    );
    assert!(
        evidence.get("store_ledger_after_failure").is_none(),
        "old store key must be gone: {stdout}"
    );
    let error = evidence["error"].as_str().expect("original error");
    assert!(
        error.contains("injected store migration failure"),
        "{error}"
    );
    // The cutover apply already committed earlier in the same run: its rows
    // and count ride along via `with_cutover_evidence`. Without that wiring
    // both asserts fail.
    let ledger_after = evidence["ledger_after"].as_array().expect("cutover ledger");
    assert_eq!(
        ledger_after.len(),
        SEED_VERSIONS.len() + pending.len(),
        "ledger_after must hold the committed cutover rows: {stdout}"
    );
    assert!(
        ledger_after.iter().any(|row| row["version"] == pending[0]),
        "newly applied cutover version missing: {stdout}"
    );
    assert_eq!(
        evidence["applied_count"].as_u64().unwrap(),
        pending.len() as u64,
        "applied_count must equal the cutover count: {stdout}"
    );
    // Database truth: cutover fully applied, store ledger still empty (the
    // failed insert rolled back its transaction).
    let mut c = fx.scratch().await?;
    let total = two_bot_cutover::staging_migrate::MIGRATOR.iter().count() as i64;
    let cutover_rows: i64 = sqlx::query_scalar("SELECT count(*) FROM public._sqlx_migrations")
        .fetch_one(&mut c)
        .await?;
    assert_eq!(cutover_rows, total, "cutover must have committed");
    let store_rows: i64 = sqlx::query_scalar("SELECT count(*) FROM public._two_bot_migrations")
        .fetch_one(&mut c)
        .await?;
    assert_eq!(store_rows, 0, "failed store insert must leave no row");
    c.close().await?;
    fx.finish().await?;
    Ok(())
}

/// The connection count must be visible to the unprivileged plan login:
/// `backend_type` is NULL for other roles' sessions without
/// `pg_read_all_stats`, so a filtered `pg_stat_activity` count sees only the
/// reader's own session. Open sessions as another role and read through the
/// plan-style RO login; the count must include them.
#[tokio::test]
#[ignore = "requires agent-testdb or the CI service"]
async fn connection_count_sees_other_roles_from_plan_login() -> TestResult {
    use two_bot_cutover::staging_migrate::CONNECTION_COUNT_SQL;

    let fx = Fixture::new().await?;
    let result = async {
        let outsider_options = PgConnectOptions::new_without_pgpass()
            .host(&fx.host)
            .port(5432)
            .username(&fx.outsider)
            .password("")
            .database(&fx.database)
            .ssl_mode(PgSslMode::Disable);
        let mut others = Vec::new();
        for _ in 0..3 {
            others.push(PgConnection::connect_with(&outsider_options).await?);
        }
        let mut reader = fx.ro_session().await?;
        let used: i64 = sqlx::query_scalar(CONNECTION_COUNT_SQL)
            .fetch_one(&mut reader)
            .await?;
        assert!(
            used >= 3,
            "plan login must see the 3 other-role sessions, counted {used}"
        );
        for conn in others {
            conn.close().await?;
        }
        reader.close().await?;
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    }
    .await;
    fx.finish().await?;
    result
}
