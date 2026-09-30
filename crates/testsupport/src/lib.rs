//! Dev-only disposable Postgres fixtures. Never depend on this crate at runtime.

use anyhow::{bail, Context, Result};
use sqlx::migrate::Migrator;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
use sqlx::PgPool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use url::Url;

/// Validate before any connection is opened. Errors never echo the URL.
///
/// Only the passwordless test principal on the named disposable service is
/// accepted. CI aliases that service as agent-testdb; no loopback URLs or libpq
/// query overrides are permitted.
pub fn guard_database_url(raw: &str) -> Result<()> {
    if raw
        .bytes()
        .any(|b| b.is_ascii_whitespace() || b.is_ascii_control())
    {
        bail!("test database URL must not contain whitespace or control characters");
    }
    let url = Url::parse(raw).map_err(|_| anyhow::anyhow!("invalid test database URL"))?;
    if !matches!(url.scheme(), "postgres" | "postgresql") {
        bail!("test database URL must use postgres or postgresql");
    }
    let host = url.host_str().unwrap_or_default();
    if host != "agent-testdb" || url.port() != Some(5432) {
        bail!("test database must use agent-testdb on explicit port 5432");
    }
    // Url normalizes an explicitly empty password to None. Compare the raw
    // authority too, so omissions, percent escapes and alternate credentials
    // cannot become equivalent through normalization.
    let authority = raw
        .split_once("://")
        .map(|(_, tail)| tail.split('/').next().unwrap_or_default());
    let expected = format!("agent_test:@{host}:5432");
    if url.username() != "agent_test" || authority != Some(expected.as_str()) {
        bail!("test database must use agent_test with an explicit empty password");
    }
    if url.query().is_some() || url.fragment().is_some() {
        bail!("test database URL must not have query parameters or fragments");
    }
    let database = url.path().strip_prefix('/').unwrap_or_default();
    let suffix = database.strip_prefix("two_bot_test_").unwrap_or_default();
    if suffix.is_empty()
        || suffix.ends_with('_')
        || database.len() > 63
        || !suffix
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        || raw
            .split_once("://")
            .and_then(|(_, tail)| tail.split_once('/').map(|(_, path)| path))
            != Some(database)
    {
        bail!("test database name must be two_bot_test_ followed by lowercase letters, digits or underscores (max 63 bytes)");
    }
    guard_environment(|name| std::env::var_os(name).is_some())
}

// SQLx populates defaults from libpq environment variables even when parsing an
// explicit URL. In particular PGOPTIONS is merged, not replaced. Fail closed
// instead of inheriting any ambient connection/credential configuration.
const PG_ENV: &[&str] = &[
    "PGHOST",
    "PGHOSTADDR",
    "PGPORT",
    "PGUSER",
    "PGPASSWORD",
    "PGDATABASE",
    "PGOPTIONS",
    "PGSERVICE",
    "PGSERVICEFILE",
    "PGPASSFILE",
    "PGSSLMODE",
    "PGSSLROOTCERT",
    "PGSSLCERT",
    "PGSSLKEY",
    "PGAPPNAME",
];

fn guard_environment(mut is_set: impl FnMut(&str) -> bool) -> Result<()> {
    if PG_ENV.iter().any(|name| is_set(name)) {
        bail!("unset libpq PG* connection/credential variables before using test fixtures");
    }
    Ok(())
}

fn connect_options(raw: &str) -> Result<PgConnectOptions> {
    guard_database_url(raw)?;
    let url = Url::parse(raw).expect("guard already parsed URL");
    Ok(PgConnectOptions::new_without_pgpass()
        .host(url.host_str().expect("guard checked host"))
        .port(5432)
        .username("agent_test")
        .password("")
        .database(url.path().trim_start_matches('/'))
        .ssl_mode(PgSslMode::Disable)
        .options([("statement_timeout", "5000ms")]))
}

static NEXT_DATABASE: AtomicU64 = AtomicU64::new(0);

fn database_name() -> String {
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_nanos();
    let sequence = NEXT_DATABASE.fetch_add(1, Ordering::Relaxed);
    format!(
        "two_bot_test_{:x}_{time:x}_{sequence:x}",
        std::process::id()
    )
}

struct Cleanup {
    admin: PgPool,
    pool: PgPool,
    name: String,
}

impl Cleanup {
    async fn close(self) -> Result<()> {
        self.pool.close().await;
        // The name is generated internally, not supplied by a caller.
        let result = sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP DATABASE \"{}\" WITH (FORCE)",
            self.name
        )))
        .execute(&self.admin)
        .await;
        self.admin.close().await;
        result.context("drop disposable test database")?;
        Ok(())
    }
}

/// Each fixture gets its own database and the caller's full migration set.
///
/// The URL names a pre-created empty test bootstrap database. It is never
/// migrated, truncated or dropped. Call `close().await` to verify teardown;
/// Drop schedules best-effort cleanup if a test panics while a runtime exists.
pub struct TestDatabase {
    cleanup: Option<Cleanup>,
}

impl TestDatabase {
    pub async fn create(raw: &str, migrations: &Migrator) -> Result<Self> {
        let options = connect_options(raw)?;
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(Duration::from_secs(10))
            .connect_with(options.clone())
            .await
            .context("connect to test bootstrap database")?;
        let name = database_name();
        if let Err(error) = sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE \"{name}\"")))
            .execute(&admin)
            .await
        {
            admin.close().await;
            return Err(error).context("create disposable test database");
        }
        // Lazy construction cannot fail to connect before the cleanup owner
        // exists, so migration/connection failures still drop the new database.
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .acquire_timeout(Duration::from_secs(10))
            .connect_lazy_with(options.database(&name).application_name(&name));
        let fixture = Self {
            cleanup: Some(Cleanup { admin, pool, name }),
        };
        if let Err(error) = migrations.run(fixture.pool()).await {
            fixture
                .close()
                .await
                .context("cleanup after migration failure")?;
            return Err(error).context("migrate disposable test database");
        }
        Ok(fixture)
    }

    pub fn pool(&self) -> &PgPool {
        &self.cleanup.as_ref().expect("fixture not closed").pool
    }

    /// A genuinely independent pool for restart and multi-worker tests.
    pub async fn independent_pool(&self) -> Result<PgPool> {
        self.pool_with_search_path("public").await
    }

    /// Set a bound search_path within this fixture, never a connection redirect.
    pub async fn pool_with_search_path(&self, path: &str) -> Result<PgPool> {
        let path = path.to_owned();
        PgPoolOptions::new()
            .max_connections(5)
            .acquire_timeout(Duration::from_secs(10))
            .after_connect(move |conn, _| {
                let path = path.clone();
                Box::pin(async move {
                    sqlx::query("SELECT set_config('search_path', $1, false)")
                        .bind(path)
                        .execute(conn)
                        .await?;
                    Ok(())
                })
            })
            .connect_with(self.pool().connect_options().as_ref().clone())
            .await
            .context("connect independent fixture pool")
    }

    pub fn name(&self) -> &str {
        &self.cleanup.as_ref().expect("fixture not closed").name
    }

    /// Await verified teardown without cancelling it if the caller is dropped.
    pub async fn close(mut self) -> Result<()> {
        let cleanup = self.cleanup.take().expect("fixture not closed");
        // The task owns cleanup before the first await. Dropping/aborting this
        // future only detaches the join handle; teardown continues while the
        // runtime lives, including when a checked-out connection delays close.
        tokio::spawn(cleanup.close())
            .await
            .context("join disposable test database teardown")?
    }
}

impl Drop for TestDatabase {
    fn drop(&mut self) {
        if let Some(cleanup) = self.cleanup.take() {
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    if let Err(error) = cleanup.close().await {
                        eprintln!("test database teardown failed: {error}");
                    }
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAFE: &str = "postgres://agent_test:@agent-testdb:5432/two_bot_test_guard";

    #[test]
    fn accepts_only_explicit_test_connections() {
        for scheme in ["postgres", "postgresql"] {
            guard_database_url(&format!(
                "{scheme}://agent_test:@agent-testdb:5432/two_bot_test_suite_123"
            ))
            .unwrap();
        }
    }

    #[test]
    fn refuses_credentials_redirects_and_non_test_databases() {
        let rejected = [
            "not a URL",
            "https://agent_test:@agent-testdb:5432/two_bot_test_guard",
            "postgres://agent_test:@production:5432/two_bot_test_guard",
            "postgres://agent_test:@staging:5432/two_bot_test_guard",
            "postgres://agent_test:@localhost:5432/two_bot_test_guard",
            "postgres://agent_test:@127.0.0.1:5432/two_bot_test_guard",
            "postgres://agent_test:@[::1]:5432/two_bot_test_guard",
            "postgres://agent_test:@agent-testdb:5433/two_bot_test_guard",
            "postgres://agent_test:@agent-testdb/two_bot_test_guard",
            "postgres://postgres:@agent-testdb:5432/two_bot_test_guard",
            "postgres://agent_test:secret@agent-testdb:5432/two_bot_test_guard",
            "postgres://agent_test@agent-testdb:5432/two_bot_test_guard",
            "postgres://agent_test:@agent-testdb:5432/postgres",
            "postgres://agent_test:@agent-testdb:5432/two_bot",
            "postgres://agent_test:@agent-testdb:5432/two_bot_test_",
            "postgres://agent_test:@agent-testdb:5432/two_bot_test_Guard",
            "postgres://agent_test:@agent-testdb:5432/two_bot_test_guard-other",
            "postgres://agent_test:@agent-testdb:5432/two_bot_test_guard/other",
            "postgres://agent_test:@agent-testdb:5432/./two_bot_test_guard",
            "postgres://agent_test:@agent-testdb:5432/two_bot_test_guard/../two_bot_test_guard",
            "postgres://agent_test:@agent-testdb:5432/two_bot_test_%67uard",
            "postgres://%61gent_test:@agent-testdb:5432/two_bot_test_guard",
            "postgres://agent_test:%00@agent-testdb:5432/two_bot_test_guard",
            "postgres://agent_test:@%61gent-testdb:5432/two_bot_test_guard",
            "postgres://agent_test:@agent-testdb,production:5432/two_bot_test_guard",
            "postgres://agent_test:@/two_bot_test_guard",
        ];
        for raw in rejected {
            assert!(
                guard_database_url(raw).is_err(),
                "accepted unsafe fixture: {raw}"
            );
        }
        for parameter in [
            "host=production",
            "hostaddr=10.0.0.1",
            "port=5433",
            "dbname=production",
            "user=postgres",
            "password=secret",
            "options=-csearch_path=public",
            "options[search_path]=public",
            "sslmode=disable",
            "sslrootcert=/tmp/cert",
            "service=production",
            "application_name=test",
            "",
        ] {
            assert!(guard_database_url(&format!("{SAFE}?{parameter}")).is_err());
        }
        for suffix in ["#", "#fragment", " ", "\n", "\t"] {
            assert!(guard_database_url(&format!("{SAFE}{suffix}")).is_err());
        }
        assert!(guard_database_url(&format!("{SAFE}{}", "a".repeat(64))).is_err());
    }

    #[test]
    fn refuses_inherited_libpq_configuration() {
        for blocked in PG_ENV {
            assert!(
                guard_environment(|name| name == *blocked).is_err(),
                "allowed {blocked}"
            );
        }
        guard_environment(|_| false).unwrap();
    }

    #[test]
    fn errors_do_not_expose_credentials() {
        let error = guard_database_url(&SAFE.replace("agent_test:", "agent_test:private_password"))
            .unwrap_err()
            .to_string();
        assert!(!error.contains("private_password"));
        assert!(!error.contains("postgres://"));
    }

    #[test]
    fn generated_names_are_unique_and_guarded() {
        let first = database_name();
        let second = database_name();
        assert_ne!(first, second);
        for name in [first, second] {
            guard_database_url(&format!("postgres://agent_test:@agent-testdb:5432/{name}"))
                .unwrap();
        }
    }
}
