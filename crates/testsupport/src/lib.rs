//! Dev-only disposable Postgres fixtures. Never depend on this crate at runtime.

use anyhow::{bail, Context, Result};
use sqlx::migrate::Migrator;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
use sqlx::PgPool;
use std::future::Future;
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

// Fixture pools keep a tight timeout; the admin connection only runs
// CREATE/DROP DATABASE, which can legitimately exceed it on a loaded runner.
const FIXTURE_STATEMENT_TIMEOUT: &str = "5000ms";
const ADMIN_DDL_STATEMENT_TIMEOUT: &str = "120s";

fn connect_options(raw: &str, statement_timeout: &str) -> Result<PgConnectOptions> {
    guard_database_url(raw)?;
    let url = Url::parse(raw).expect("guard already parsed URL");
    Ok(PgConnectOptions::new_without_pgpass()
        .host(url.host_str().expect("guard checked host"))
        .port(5432)
        .username("agent_test")
        .password("")
        .database(url.path().trim_start_matches('/'))
        .ssl_mode(PgSslMode::Disable)
        .options([("statement_timeout", statement_timeout)]))
}

// A seconds-long agent-testdb DNS blip failed every test in one binary with no
// retry (run 39197217315 attempt 1: 7x bootstrap-connect "Temporary failure in
// name resolution", 1x teardown EOF). Connect-level retry lives in
// `retry_on_transient`/`with_bootstrap_retries`; the statement layer below
// absorbs a blip that kills a CREATE/migrate connection instead of
// failing the suite on the first lost packet.
const SETUP_DDL_RETRY_DELAYS: [Duration; 2] = [Duration::from_secs(1), Duration::from_secs(2)];

/// Run `op` until it succeeds or `delays` run out, sleeping one delay between
/// attempts. Returns the last error when every attempt fails. Each retry is
/// one stderr line, so a passing suite still shows the blip it absorbed.
async fn retry_with_backoff<T, E, F, Fut>(
    what: &str,
    delays: &[Duration],
    mut op: F,
) -> std::result::Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = std::result::Result<T, E>>,
    E: std::fmt::Display,
{
    let mut delays = delays.iter();
    loop {
        match op().await {
            Ok(value) => return Ok(value),
            Err(error) => match delays.next() {
                Some(delay) => {
                    eprintln!("{what} failed, retrying in {delay:?}: {error}");
                    tokio::time::sleep(*delay).await;
                }
                None => return Err(error),
            },
        }
    }
}

/// A setup CREATE whose connection died after the server committed leaves the
/// database behind: the retry then reports duplicate_database (42P04), which
/// proves the setup goal instead of failing it.
fn is_database_already_exists(error: &sqlx::Error) -> bool {
    error
        .as_database_error()
        .and_then(|error| error.code())
        .is_some_and(|code| code == "42P04")
}

/// Run CREATE DATABASE, absorbing a blip that kills the admin connection. Each
/// attempt takes the DDL gate; the backoff sleep never holds it.
async fn create_database_with_retry(admin: &PgPool, name: &str, delays: &[Duration]) -> Result<()> {
    retry_with_backoff("create disposable test database", delays, || async {
        let _ddl = DATABASE_DDL.lock().await;
        match sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE \"{name}\"")))
            .execute(admin)
            .await
        {
            Ok(_) => Ok(()),
            Err(error) if is_database_already_exists(&error) => Ok(()),
            Err(error) => Err(anyhow::Error::from(error)),
        }
    })
    .await
}

/// Run the fixture migrations, absorbing a blip that breaks the pool's first
/// new TCP connections. A persistently broken migration fails after the
/// bounded backoff instead of hanging the suite.
async fn run_migrations_with_retry(
    pool: &PgPool,
    migrations: &Migrator,
    delays: &[Duration],
) -> Result<()> {
    retry_with_backoff("migrate disposable test database", delays, || async {
        migrations.run(pool).await.map_err(anyhow::Error::from)
    })
    .await
}

// CREATE/DROP DATABASE can wait for checkpoints. Queue fixture lifecycle DDL
// outside the unchanged SQL deadline instead of making concurrent drops race it.
// Cargo runs test binaries sequentially; tests inside each binary share this gate.
static DATABASE_DDL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
static NEXT_DATABASE: AtomicU64 = AtomicU64::new(0);

// A DNS blip on the disposable Postgres service fails every fixture created
// while it lasts (each connect fails fast, so a whole suite goes red in
// seconds). Retry every fixture-setup connect on transport errors only, with
// exponential backoff from this base; the sleeps below total ~7.5s.
const BOOTSTRAP_CONNECT_ATTEMPTS: u32 = 5;
const BOOTSTRAP_CONNECT_RETRY_BASE: Duration = Duration::from_millis(500);

/// Only transport-level failures are retried: DNS blips and refused/reset
/// connections. Auth, TLS, protocol and config errors fail immediately, so a
/// misconfigured fixture never burns minutes retrying a certain failure.
fn is_transient_connect_error(error: &sqlx::Error) -> bool {
    matches!(error, sqlx::Error::Io(_))
}

async fn retry_on_transient<T, F, Fut>(
    attempts: u32,
    base_delay: Duration,
    mut op: F,
) -> Result<T, sqlx::Error>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, sqlx::Error>>,
{
    let mut attempt = 0;
    loop {
        match op().await {
            Ok(value) => return Ok(value),
            Err(error) if attempt + 1 < attempts && is_transient_connect_error(&error) => {
                eprintln!(
                    "test bootstrap connect failed (attempt {}/{}): {error}; retrying",
                    attempt + 1,
                    attempts
                );
                tokio::time::sleep(base_delay * 2u32.pow(attempt)).await;
                attempt += 1;
            }
            Err(error) => return Err(error),
        }
    }
}

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
    independent_pools: std::sync::Mutex<Vec<PgPool>>,
    name: String,
}

async fn prepare_cleanup_connection(conn: &mut sqlx::PgConnection) -> Result<()> {
    // DROP DATABASE waits for a checkpoint; ordinary fixture queries keep 5s.
    sqlx::query("SET statement_timeout = '30s'")
        .execute(conn)
        .await
        .context("set disposable database teardown timeout")?;
    Ok(())
}

/// Substrings marking a teardown failure as transient hosted-runner
/// infrastructure (DNS, dropped connection) rather than a deterministic
/// teardown bug (unknown database, permission). Anything outside this list
/// fails fast and is never retried.
fn is_transient_teardown_error(error: &anyhow::Error) -> bool {
    let text = format!("{error:#}").to_lowercase();
    const MARKERS: &[&str] = &[
        "temporary failure in name resolution",
        "failed to lookup address information",
        "name or service not known",
        "connection refused",
        "connection reset",
        "connection closed",
        "broken pipe",
        "unexpected eof",
        "got 0 bytes",
        "expected to read",
        "could not connect",
        "server closed the connection",
        "connection timed out",
        "timed out",
    ];
    MARKERS.iter().any(|marker| text.contains(marker))
}

/// A retry that finds the database already gone means an earlier attempt
/// committed its DROP server-side before the client saw EOF. Fixture names
/// are unique per process/time/sequence and only this Cleanup drops them,
/// so "does not exist" on attempt 2+ is success, never a bug.
fn is_teardown_already_dropped(error: &anyhow::Error) -> bool {
    format!("{error:#}")
        .to_lowercase()
        .contains("does not exist")
}

/// Run a teardown operation until it succeeds, retrying only transient
/// hosted-runner infrastructure failures. The attempt budget is explicit so
/// unit tests pin the loop: a transient-then-success operation is called
/// until success, a deterministic error is called exactly once. A
/// "does not exist" error after the first attempt means an earlier DROP
/// already committed, so it returns success; the first attempt stays strict
/// so wrong-name teardown bugs still fail fast.
async fn with_teardown_retries<F, Fut>(attempts: usize, mut operation: F) -> Result<()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    let attempts = attempts.max(1);
    let mut attempt = 0;
    loop {
        attempt += 1;
        match operation().await {
            Ok(()) => return Ok(()),
            Err(error) if attempt > 1 && is_teardown_already_dropped(&error) => {
                eprintln!(
                    "test-database teardown already dropped on attempt {attempt}/{attempts}; treating as success"
                );
                return Ok(());
            }
            Err(error) if attempt < attempts && is_transient_teardown_error(&error) => {
                eprintln!(
                    "transient test-database teardown failure (attempt {attempt}/{attempts}): {error:#}; retrying"
                );
                tokio::time::sleep(Duration::from_millis(250u64 << (attempt - 1))).await;
            }
            Err(error) => return Err(error),
        }
    }
}

impl Cleanup {
    async fn drop_once(admin: &PgPool, name: &str) -> Result<()> {
        let _ddl = DATABASE_DDL.lock().await;
        let mut conn = admin.acquire().await?;
        prepare_cleanup_connection(&mut conn).await?;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP DATABASE \"{}\" WITH (FORCE)",
            name
        )))
        .execute(&mut *conn)
        .await?;
        Ok(())
    }

    async fn close(self) -> Result<()> {
        let Self {
            admin,
            pool,
            independent_pools,
            name,
        } = self;
        for pool in independent_pools.into_inner().unwrap() {
            pool.close().await;
        }
        pool.close().await;
        // The name is generated internally, not supplied by a caller.
        // Hosted runners occasionally lose the service hostname for seconds;
        // the same blip that fails bootstrap connects can also kill an
        // in-flight DROP with EOF. Retry only transient teardown failures;
        // anything else fails fast with its original context. The DDL guard
        // is per attempt inside drop_once, never held across backoff sleep.
        const ATTEMPTS: usize = 4;
        let result = with_teardown_retries(ATTEMPTS, || Self::drop_once(&admin, &name)).await;
        admin.close().await;
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

/// Substrings marking a bootstrap failure as transient hosted-runner
/// infrastructure (DNS, dropped connection, refused/reset) rather than a
/// deterministic guard, auth or schema error. Anything outside this list
/// fails fast and is never retried.
fn is_transient_bootstrap_error(error: &anyhow::Error) -> bool {
    let text = format!("{error:#}").to_lowercase();
    const MARKERS: &[&str] = &[
        "temporary failure in name resolution",
        "failed to lookup address information",
        "name or service not known",
        "connection refused",
        "connection reset",
        "connection closed",
        "broken pipe",
        "unexpected eof",
        "got 0 bytes",
        "expected to read",
        "could not connect",
        "server closed the connection",
        "connection timed out",
        "timed out",
    ];
    MARKERS.iter().any(|marker| text.contains(marker))
}

/// Run a bootstrap operation until it succeeds, retrying only transient
/// hosted-runner infrastructure failures. The attempt budget is explicit so
/// unit tests pin the loop: a transient-then-success operation is called
/// until success, a deterministic error is called exactly once.
async fn with_bootstrap_retries<T, F, Fut>(attempts: usize, mut operation: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let attempts = attempts.max(1);
    let mut attempt = 0;
    loop {
        attempt += 1;
        match operation().await {
            Ok(value) => return Ok(value),
            Err(error) if attempt < attempts && is_transient_bootstrap_error(&error) => {
                eprintln!(
                    "transient test-database bootstrap failure (attempt {attempt}/{attempts}): {error:#}; retrying"
                );
                tokio::time::sleep(Duration::from_millis(250u64 << (attempt - 1))).await;
            }
            Err(error) => return Err(error),
        }
    }
}

impl TestDatabase {
    pub async fn create(raw: &str, migrations: &Migrator) -> Result<Self> {
        // Validate once and fail fast: guard/auth errors are deterministic and
        // must never be retried.
        connect_options(raw, ADMIN_DDL_STATEMENT_TIMEOUT)?;
        // Hosted runners occasionally lose the service hostname for seconds
        // (run 39186954211: every internal_member_store fixture failed with
        // EAI_AGAIN/EOF after all earlier store steps passed on the same
        // host). Retry only transient bootstrap failures; anything else fails
        // fast with its original context.
        const ATTEMPTS: usize = 4;
        with_bootstrap_retries(ATTEMPTS, || Self::create_once(raw, migrations)).await
    }

    async fn create_once(raw: &str, migrations: &Migrator) -> Result<Self> {
        let options = connect_options(raw, FIXTURE_STATEMENT_TIMEOUT)?;
        let admin_options = connect_options(raw, ADMIN_DDL_STATEMENT_TIMEOUT)?;
        let admin = retry_on_transient(
            BOOTSTRAP_CONNECT_ATTEMPTS,
            BOOTSTRAP_CONNECT_RETRY_BASE,
            || {
                PgPoolOptions::new()
                    .max_connections(1)
                    .acquire_timeout(Duration::from_secs(10))
                    .connect_with(admin_options.clone())
            },
        )
        .await
        .context("connect to test bootstrap database")?;
        let name = database_name();
        if let Err(error) = create_database_with_retry(&admin, &name, &SETUP_DDL_RETRY_DELAYS).await
        {
            admin.close().await;
            return Err(error).context("create disposable test database");
        }
        // Lazy construction cannot fail to connect before the cleanup owner
        // exists, so migration/connection failures still drop the new database.
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .acquire_timeout(Duration::from_secs(10))
            .connect_lazy_with(
                options
                    .database(&name)
                    .application_name(&name)
                    .options([("statement_timeout", "5000ms")]),
            );
        let fixture = Self {
            cleanup: Some(Cleanup {
                admin,
                pool,
                independent_pools: std::sync::Mutex::new(Vec::new()),
                name,
            }),
        };
        // The pool above is lazy, so its first acquire is the fixture's first
        // real connection to the new database. Retry it on transport errors:
        // the cleanup owner already exists, so a persistent failure still
        // drops the created database.
        if let Err(error) = retry_on_transient(
            BOOTSTRAP_CONNECT_ATTEMPTS,
            BOOTSTRAP_CONNECT_RETRY_BASE,
            || async { fixture.pool().acquire().await.map(|_| ()) },
        )
        .await
        {
            fixture
                .close()
                .await
                .context("cleanup after fixture connect failure")?;
            return Err(error).context("connect to disposable test database");
        }
        if let Err(error) =
            run_migrations_with_retry(fixture.pool(), migrations, &SETUP_DDL_RETRY_DELAYS).await
        {
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
        let pool = retry_on_transient(
            BOOTSTRAP_CONNECT_ATTEMPTS,
            BOOTSTRAP_CONNECT_RETRY_BASE,
            || {
                let path = path.to_owned();
                let options = self.pool().connect_options().as_ref().clone();
                async move {
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
                        .connect_with(options)
                        .await
                }
            },
        )
        .await
        .context("connect independent fixture pool")?;
        self.cleanup
            .as_ref()
            .expect("fixture not closed")
            .independent_pools
            .lock()
            .unwrap()
            .push(pool.clone());
        Ok(pool)
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

/// Every `event_store` column a reviewed capture slice may add: the S1 core
/// plus the member, voice, presence, message/reaction, thread,
/// stage/interaction/audit and invite/scheduled-event slices (the fence table
/// in `docs/event-store.md`).
/// The table is shared and grows by `ADD COLUMN IF NOT EXISTS`, so a slice
/// suite checks "my columns are present and nothing outside this list
/// exists" instead of an exact set that breaks when a sibling lands. A new
/// slice extends this list in its own PR; a raw-payload or free-text column
/// is in no slice and fails every suite.
pub const EVENT_STORE_ALLOWED_COLUMNS: &[&str] = &[
    "id",
    "event_type",
    "guild_id",
    "channel_id",
    "message_id",
    "user_id",
    "event_time",
    "received_at",
    "dedup_key",
    "premium_since",
    "nick_changed",
    "roles",
    "session_id",
    "mute",
    "deaf",
    "self_mute",
    "self_deaf",
    "self_video",
    "self_stream",
    "suppress",
    "presence_status",
    "activity_type",
    "application_id",
    "thread_id",
    "parent_id",
    "archived",
    "locked",
    "invitable",
    "emoji_id",
    "emoji_name",
    "stage_instance_id",
    "privacy_level",
    "interaction_id",
    "interaction_type",
    "command_id",
    "audit_log_entry_id",
    "action_type",
    "actor_id",
    "target_id",
    "invite_code_hash",
    "inviter_id",
    "scheduled_event_id",
    "event_status",
    "entity_type",
];

/// Allowlist check for a shared `event_store`: every `required` column is
/// present and every actual column is in [`EVENT_STORE_ALLOWED_COLUMNS`].
/// Panics with the offending names so a suite failure is self-explaining.
pub fn assert_event_store_columns(actual: &[String], required: &[&str]) {
    for column in required {
        assert!(
            actual.iter().any(|name| name == column),
            "event_store is missing the {column} column; has {actual:?}"
        );
    }
    let unreviewed: Vec<&String> = actual
        .iter()
        .filter(|name| !EVENT_STORE_ALLOWED_COLUMNS.contains(&name.as_str()))
        .collect();
    assert!(
        unreviewed.is_empty(),
        "event_store has columns outside the reviewed allowlist {unreviewed:?}; \
         add a column only through a reviewed slice that extends \
         EVENT_STORE_ALLOWED_COLUMNS and docs/event-store.md"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// Fast retry schedule for wiring tests: 200ms total, so elapsed time
    /// proves the retries ran without a 15s wall-clock wait.
    const WIRING_DELAYS: [Duration; 2] = [Duration::from_millis(100), Duration::from_millis(100)];

    /// A guard-valid bootstrap URL naming a database nobody creates, derived
    /// from the live bootstrap so the failure is a fast undefined_catalog.
    /// Returns None (skip the test) without a configured bootstrap database.
    fn missing_bootstrap_url() -> Option<String> {
        let os = std::env::var_os("TWO_TEST_DATABASE_URL")?;
        let base = os
            .into_string()
            .expect("invalid test bootstrap configuration");
        // Splice the path by hand: Url serialization drops the explicit empty
        // password colon, which the guard requires byte-for-byte.
        let (head, _) = base
            .rsplit_once('/')
            .expect("test bootstrap URL names a database");
        let missing = format!("{head}/two_bot_test_missing_bootstrap_probe");
        guard_database_url(&missing).expect("probe bootstrap URL stays guarded");
        Some(missing)
    }

    fn names(columns: &[&str]) -> Vec<String> {
        columns.iter().map(|name| name.to_string()).collect()
    }

    #[test]
    fn event_store_allowlist_accepts_a_subset_with_the_required_columns() {
        assert_event_store_columns(&names(&["id", "dedup_key", "roles"]), &["id", "roles"]);
        assert_event_store_columns(&names(EVENT_STORE_ALLOWED_COLUMNS), &["mute", "roles"]);
    }

    #[test]
    #[should_panic(expected = "outside the reviewed allowlist")]
    fn event_store_allowlist_rejects_a_raw_payload_column() {
        assert_event_store_columns(&names(&["id", "dedup_key", "raw_payload"]), &["id"]);
    }

    #[test]
    #[should_panic(expected = "missing the roles column")]
    fn event_store_allowlist_requires_the_slice_columns() {
        assert_event_store_columns(&names(&["id", "dedup_key"]), &["roles"]);
    }

    #[test]
    fn bootstrap_retry_covers_transient_infra_but_not_guard_errors() {
        for transient in [
            "connect to test bootstrap database: failed to lookup address information: \
             Temporary failure in name resolution",
            "create disposable test database: expected to read 5 bytes, got 0 bytes at EOF",
            "pool timed out while waiting for an open connection",
            "connection refused",
        ] {
            assert!(
                is_transient_bootstrap_error(&anyhow::anyhow!("{transient}")),
                "should retry: {transient}"
            );
        }
        for deterministic in [
            "test database must use agent-testdb on explicit port 5432",
            "invalid test database URL",
            "test database URL must not contain whitespace or control characters",
            "relation \"event_store\" does not exist",
        ] {
            assert!(
                !is_transient_bootstrap_error(&anyhow::anyhow!("{deterministic}")),
                "must fail fast: {deterministic}"
            );
        }
    }

    #[tokio::test]
    async fn bootstrap_retry_retries_transient_until_success() {
        use std::sync::{Arc, Mutex};
        let calls = Arc::new(Mutex::new(0usize));
        let result = with_bootstrap_retries(3, || {
            let calls = calls.clone();
            async move {
                let mut calls = calls.lock().unwrap();
                *calls += 1;
                let attempt = *calls;
                drop(calls);
                if attempt < 3 {
                    Err::<u8, _>(anyhow::anyhow!("connection refused"))
                } else {
                    Ok(7u8)
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(result, 7);
        assert_eq!(*calls.lock().unwrap(), 3);
    }

    #[tokio::test]
    async fn bootstrap_retry_fails_fast_on_deterministic_error() {
        use std::sync::{Arc, Mutex};
        let calls = Arc::new(Mutex::new(0usize));
        let result = with_bootstrap_retries(4, || {
            let calls = calls.clone();
            async move {
                *calls.lock().unwrap() += 1;
                Err::<u8, _>(anyhow::anyhow!(
                    "test database must use agent-testdb on explicit port 5432"
                ))
            }
        })
        .await;
        assert!(result.is_err());
        assert_eq!(*calls.lock().unwrap(), 1);
    }

    const SAFE: &str = "postgres://agent_test:@agent-testdb:5432/two_bot_test_guard";

    #[tokio::test]
    async fn administrative_timeout_does_not_relax_fixture_queries() {
        let raw = match std::env::var_os("TWO_TEST_DATABASE_URL") {
            Some(os) => os
                .into_string()
                .expect("invalid test bootstrap configuration"),
            None => return,
        };
        let fixture = TestDatabase::create(&raw, &sqlx::migrate!("./tests/migrations"))
            .await
            .unwrap();
        let admin = &fixture.cleanup.as_ref().unwrap().admin;
        let timeout: String = sqlx::query_scalar("SHOW statement_timeout")
            .fetch_one(admin)
            .await
            .unwrap();
        assert_eq!(timeout, "2min");
        let peer = fixture.independent_pool().await.unwrap();
        for pool in [fixture.pool(), &peer] {
            let timeout: String = sqlx::query_scalar("SHOW statement_timeout")
                .fetch_one(pool)
                .await
                .unwrap();
            assert_eq!(timeout, "5s");
        }
        peer.close().await;
        fixture.close().await.unwrap();
    }

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

    #[tokio::test]
    async fn lifecycle_ddl_waits_for_the_gate_before_starting_sql() {
        use std::future::Future;
        use std::task::Poll;

        let first = DATABASE_DDL.lock().await;
        let second = DATABASE_DDL.lock();
        tokio::pin!(second);
        let state = std::future::poll_fn(|cx| Poll::Ready(second.as_mut().poll(cx))).await;
        assert!(
            state.is_pending(),
            "concurrent DDL entered the timed statement"
        );
        drop(first);
        let _second = second.await;
    }

    #[tokio::test]
    async fn teardown_budget_is_finite_and_separate_from_fixture_queries() {
        let url = match std::env::var_os("TWO_TEST_DATABASE_URL") {
            Some(os) => os
                .into_string()
                .expect("invalid test bootstrap configuration"),
            None => return,
        };
        let db = TestDatabase::create(&url, &sqlx::migrate!("./tests/migrations"))
            .await
            .unwrap();
        let mut conn = db.cleanup.as_ref().unwrap().admin.acquire().await.unwrap();
        sqlx::query("SET statement_timeout = '100ms'")
            .execute(&mut *conn)
            .await
            .unwrap();
        prepare_cleanup_connection(&mut conn).await.unwrap();
        let timeout: String = sqlx::query_scalar("SHOW statement_timeout")
            .fetch_one(&mut *conn)
            .await
            .unwrap();
        assert_eq!(timeout, "30s");
        sqlx::query("SELECT pg_sleep(0.2)")
            .execute(&mut *conn)
            .await
            .unwrap();
        drop(conn);

        let peer = db.independent_pool().await.unwrap();
        for pool in [db.pool(), &peer] {
            let timeout: String = sqlx::query_scalar("SHOW statement_timeout")
                .fetch_one(pool)
                .await
                .unwrap();
            assert_eq!(timeout, "5s");
        }
        peer.close().await;
        db.close().await.unwrap();
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

    #[tokio::test]
    async fn backoff_retry_returns_the_first_success() {
        use std::sync::atomic::{AtomicU64, Ordering};
        let attempts = AtomicU64::new(0);
        let result = retry_with_backoff("test op", &[Duration::ZERO, Duration::ZERO], || {
            let attempt = attempts.fetch_add(1, Ordering::SeqCst) + 1;
            async move {
                if attempt < 3 {
                    Err::<&str, &str>("boom")
                } else {
                    Ok("done")
                }
            }
        })
        .await;
        assert_eq!(result, Ok("done"));
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn backoff_retry_gives_up_after_the_last_delay() {
        use std::sync::atomic::{AtomicU64, Ordering};
        let attempts = AtomicU64::new(0);
        let result = retry_with_backoff("test op", &[Duration::ZERO, Duration::ZERO], || {
            attempts.fetch_add(1, Ordering::SeqCst);
            async { Err::<(), &str>("boom") }
        })
        .await;
        assert_eq!(result, Err("boom"));
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn teardown_retry_covers_transient_infra_but_not_teardown_bugs() {
        for transient in [
            "drop disposable test database: error communicating with database: \
             expected to read 5 bytes, got 0 bytes at EOF",
            "server closed the connection unexpectedly",
            "connection reset by peer",
            "pool timed out while waiting for an open connection",
        ] {
            assert!(
                is_transient_teardown_error(&anyhow::anyhow!("{transient}")),
                "should retry: {transient}"
            );
        }
        for deterministic in [
            "database \"two_bot_test_deadbeef\" does not exist",
            "permission denied to drop database",
            "cannot drop the currently open database",
            "set disposable database teardown timeout: statement timeout",
        ] {
            assert!(
                !is_transient_teardown_error(&anyhow::anyhow!("{deterministic}")),
                "must fail fast: {deterministic}"
            );
        }
    }

    #[tokio::test]
    async fn teardown_retry_retries_transient_until_success() {
        use std::sync::{Arc, Mutex};
        let calls = Arc::new(Mutex::new(0usize));
        with_teardown_retries(4, || {
            let calls = calls.clone();
            async move {
                let mut calls = calls.lock().unwrap();
                *calls += 1;
                let attempt = *calls;
                drop(calls);
                if attempt < 3 {
                    Err(anyhow::anyhow!("connection reset by peer"))
                } else {
                    Ok(())
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(*calls.lock().unwrap(), 3);
    }

    #[tokio::test]
    async fn teardown_retry_fails_fast_on_deterministic_error() {
        use std::sync::{Arc, Mutex};
        let calls = Arc::new(Mutex::new(0usize));
        let result = with_teardown_retries(4, || {
            let calls = calls.clone();
            async move {
                *calls.lock().unwrap() += 1;
                Err(anyhow::anyhow!("permission denied to drop database"))
            }
        })
        .await;
        assert!(result.is_err());
        assert_eq!(*calls.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn teardown_retry_exhausts_attempts_on_persistent_transient() {
        use std::sync::{Arc, Mutex};
        let calls = Arc::new(Mutex::new(0usize));
        let result = with_teardown_retries(4, || {
            let calls = calls.clone();
            async move {
                *calls.lock().unwrap() += 1;
                Err(anyhow::anyhow!(
                    "expected to read 5 bytes, got 0 bytes at EOF"
                ))
            }
        })
        .await;
        assert!(result.is_err());
        assert_eq!(*calls.lock().unwrap(), 4);
    }

    #[tokio::test]
    async fn teardown_retry_treats_already_dropped_as_success_after_transient() {
        use std::sync::{Arc, Mutex};
        let calls = Arc::new(Mutex::new(0usize));
        with_teardown_retries(4, || {
            let calls = calls.clone();
            async move {
                let mut calls = calls.lock().unwrap();
                *calls += 1;
                let attempt = *calls;
                drop(calls);
                if attempt == 1 {
                    Err(anyhow::anyhow!(
                        "expected to read 5 bytes, got 0 bytes at EOF"
                    ))
                } else {
                    Err(anyhow::anyhow!(
                        "database \"two_bot_test_deadbeef\" does not exist"
                    ))
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(*calls.lock().unwrap(), 2);
    }

    #[tokio::test]
    async fn teardown_retry_fails_fast_when_first_attempt_finds_nothing() {
        use std::sync::{Arc, Mutex};
        let calls = Arc::new(Mutex::new(0usize));
        let result = with_teardown_retries(4, || {
            let calls = calls.clone();
            async move {
                *calls.lock().unwrap() += 1;
                Err(anyhow::anyhow!(
                    "database \"two_bot_test_deadbeef\" does not exist"
                ))
            }
        })
        .await;
        assert!(result.is_err());
        assert_eq!(*calls.lock().unwrap(), 1);
    }

    fn transient_io_error() -> sqlx::Error {
        sqlx::Error::Io(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "Temporary failure in name resolution",
        ))
    }

    /// The production retry budget covers a short DNS blip on every
    /// fixture-setup connect (admin, fixture first-acquire, independent pool).
    #[test]
    fn bootstrap_retry_budget_covers_a_dns_blip() {
        assert_eq!(BOOTSTRAP_CONNECT_ATTEMPTS, 5);
        assert_eq!(BOOTSTRAP_CONNECT_RETRY_BASE, Duration::from_millis(500));
    }

    #[tokio::test]
    async fn transient_connect_errors_are_retried_until_success() {
        let mut calls = 0;
        let result = retry_on_transient(BOOTSTRAP_CONNECT_ATTEMPTS, Duration::ZERO, || {
            calls += 1;
            let attempt = calls;
            async move {
                if attempt < 3 {
                    Err(transient_io_error())
                } else {
                    Ok("connected")
                }
            }
        })
        .await;
        assert_eq!(result.unwrap(), "connected");
        assert_eq!(calls, 3);
    }

    #[tokio::test]
    async fn persistent_transient_errors_give_up_after_max_attempts() {
        let mut calls = 0;
        let result: Result<(), sqlx::Error> =
            retry_on_transient(BOOTSTRAP_CONNECT_ATTEMPTS, Duration::ZERO, || {
                calls += 1;
                async { Err(transient_io_error()) }
            })
            .await;
        assert!(result.is_err());
        assert_eq!(calls, BOOTSTRAP_CONNECT_ATTEMPTS);
    }

    /// Backoff sleeps grow exponentially with no sleep after the final
    /// attempt. Lower bound only: loaded CI timers fire late, never early
    /// enough to matter at this scale.
    #[tokio::test]
    async fn retry_backoff_sleeps_grow_exponentially() {
        let start = std::time::Instant::now();
        let result: Result<(), sqlx::Error> =
            retry_on_transient(4, Duration::from_millis(50), || async {
                Err(transient_io_error())
            })
            .await;
        assert!(result.is_err());
        // Ladder is 50ms, 100ms, 200ms; assert well under the 350ms total so
        // timer granularity cannot flake, while a flat or missing backoff
        // (150ms or ~0ms) still fails.
        assert!(
            start.elapsed() >= Duration::from_millis(300),
            "backoff did not follow the 50/100/200ms ladder"
        );
    }

    /// Stand-in database error carrying a message and an optional SQLSTATE
    /// code: proves CREATE recognizes a lost-then-created database and the
    /// connect classifier fails fast on auth errors.
    #[derive(Debug)]
    struct FakeDbError {
        message: &'static str,
        code: Option<&'static str>,
    }

    impl std::fmt::Display for FakeDbError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(self.message)
        }
    }

    impl std::error::Error for FakeDbError {}

    impl sqlx::error::DatabaseError for FakeDbError {
        fn message(&self) -> &str {
            self.message
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

        fn kind(&self) -> sqlx::error::ErrorKind {
            sqlx::error::ErrorKind::Other
        }
    }

    #[test]
    fn create_treats_a_duplicate_database_as_created() {
        let duplicate = sqlx::Error::Database(Box::new(FakeDbError {
            message: "database already exists",
            code: Some("42P04"),
        }));
        assert!(is_database_already_exists(&duplicate));
        let missing = sqlx::Error::Database(Box::new(FakeDbError {
            message: "database does not exist",
            code: Some("3D000"),
        }));
        assert!(!is_database_already_exists(&missing));
        let eof = sqlx::Error::Io(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "expected to read 5 bytes, got 0 bytes at EOF",
        ));
        assert!(!is_database_already_exists(&eof));
    }

    #[test]
    fn retry_budgets_are_bounded_and_enabled() {
        assert_eq!(
            SETUP_DDL_RETRY_DELAYS,
            [Duration::from_secs(1), Duration::from_secs(2)]
        );
    }

    #[tokio::test]
    async fn create_database_retry_treats_a_duplicate_as_created() {
        let raw = match std::env::var_os("TWO_TEST_DATABASE_URL") {
            Some(os) => os
                .into_string()
                .expect("invalid test bootstrap configuration"),
            None => return,
        };
        let fixture = TestDatabase::create(&raw, &sqlx::migrate!("./tests/migrations"))
            .await
            .unwrap();
        create_database_with_retry(
            &fixture.cleanup.as_ref().unwrap().admin,
            fixture.name(),
            &[Duration::ZERO],
        )
        .await
        .expect("re-creating the fixture database must succeed via 42P04");
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn create_database_retry_gives_up_after_the_last_delay() {
        let Some(missing) = missing_bootstrap_url() else {
            return;
        };
        // A lazy pool that can never connect: every CREATE attempt fails fast
        // without creating anything, so the elapsed backoff proves the
        // retries ran.
        let dead = PgPoolOptions::new().max_connections(1).connect_lazy_with(
            connect_options(&missing, ADMIN_DDL_STATEMENT_TIMEOUT).expect("probe options parse"),
        );
        let start = Instant::now();
        create_database_with_retry(&dead, "two_bot_test_never_created_probe", &WIRING_DELAYS)
            .await
            .expect_err("CREATE against a dead admin must keep failing");
        assert!(
            start.elapsed() >= Duration::from_millis(100),
            "CREATE returned in {:?} without running the 200ms retry backoff",
            start.elapsed()
        );
        dead.close().await;
    }

    #[tokio::test]
    async fn migrations_retry_gives_up_after_the_last_delay() {
        let raw = match std::env::var_os("TWO_TEST_DATABASE_URL") {
            Some(os) => os
                .into_string()
                .expect("invalid test bootstrap configuration"),
            None => return,
        };
        let fixture = TestDatabase::create(&raw, &sqlx::migrate!("./tests/migrations"))
            .await
            .unwrap();
        let broken = sqlx::migrate!("./tests/broken_migrations");
        let start = Instant::now();
        run_migrations_with_retry(fixture.pool(), &broken, &WIRING_DELAYS)
            .await
            .expect_err("a broken migration must keep failing");
        assert!(
            start.elapsed() >= Duration::from_millis(100),
            "migrations returned in {:?} without running the 200ms retry backoff",
            start.elapsed()
        );
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn public_create_still_retries_migrations_past_the_first_production_delay() {
        let raw = match std::env::var_os("TWO_TEST_DATABASE_URL") {
            Some(os) => os
                .into_string()
                .expect("invalid test bootstrap configuration"),
            None => return,
        };
        // Pins the public TestDatabase::create delegate to the 1s/2s
        // production migration backoff: a broken migration keeps failing, so
        // at 1.5s the first delay is consumed and a later attempt is in
        // flight, and the timeout must elapse. Emptying the delegate delays
        // returns the error in milliseconds and the timeout observes it. The
        // cancelled create drops its fixture, whose Drop schedules
        // best-effort teardown of the created database.
        let broken = sqlx::migrate!("./tests/broken_migrations");
        let probe = tokio::time::timeout(
            Duration::from_millis(1500),
            TestDatabase::create(&raw, &broken),
        )
        .await;
        assert!(
            probe.is_err(),
            "public TestDatabase::create returned before the 1s production migration delay elapsed"
        );
    }

    async fn assert_fails_fast(error: sqlx::Error) {
        let mut slot = Some(error);
        let mut calls = 0;
        let result: Result<(), sqlx::Error> =
            retry_on_transient(BOOTSTRAP_CONNECT_ATTEMPTS, Duration::ZERO, || {
                calls += 1;
                let error = slot.take().expect("non-transient error was retried");
                async move { Err(error) }
            })
            .await;
        assert!(result.is_err());
        assert_eq!(calls, 1);
    }

    #[tokio::test]
    async fn non_transient_errors_are_not_retried() {
        assert_fails_fast(sqlx::Error::from(FakeDbError {
            message: "password authentication failed",
            code: None,
        }))
        .await;
        assert_fails_fast(sqlx::Error::Protocol("unexpected auth message".to_string())).await;
        assert_fails_fast(sqlx::Error::Configuration(
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "bad connect option").into(),
        ))
        .await;
    }
}
