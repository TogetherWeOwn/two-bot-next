//! Validate before SQLx parses a credential-bearing URL. SQLx 0.9 logs both
//! key and value for unrecognized query parameters at WARN, even if parsing
//! later fails. An error sanitizer cannot retract that dependency diagnostic.
//!
//! The same holds for the driver's passfile lookup: parse through
//! [`connect_options`], which keeps the credential-source semantics while
//! dropping the passfile target's diagnostics for the synchronous parse.

#[cfg(feature = "db")]
use sqlx::postgres::PgConnectOptions;
#[cfg(feature = "db")]
use std::str::FromStr;
#[cfg(feature = "db")]
use tracing::dispatcher::Dispatch;
#[cfg(feature = "db")]
use tracing::span::{Attributes, Id, Record};
#[cfg(feature = "db")]
use tracing::{Event, Metadata, Subscriber};

/// Reject unsupported parameters without passing their values to SQLx or logs.
/// Supported keys mirror the pinned sqlx-postgres 0.9 URL parser; re-audit this
/// allowlist when upgrading SQLx. Known parameter values remain SQLx's concern.
/// `channel_binding` is a libpq compatibility option: accepted here, but removed
/// by [`connect_options`] because SQLx does not implement channel binding.
pub fn validate(raw: &str) -> Result<(), &'static str> {
    let url = url::Url::parse(raw).map_err(|_| "invalid database URL")?;
    for (key, _) in url.query_pairs() {
        let supported = matches!(
            key.as_ref(),
            "sslmode"
                | "ssl-mode"
                | "sslrootcert"
                | "ssl-root-cert"
                | "ssl-ca"
                | "sslcert"
                | "ssl-cert"
                | "sslkey"
                | "ssl-key"
                | "statement-cache-capacity"
                | "host"
                | "hostaddr"
                | "port"
                | "dbname"
                | "user"
                | "password"
                | "application_name"
                | "channel_binding"
                | "options"
        ) || (key.starts_with("options[") && key.ends_with(']'));
        if !supported {
            return Err("unsupported database URL parameter");
        }
    }
    Ok(())
}

/// Database names that always name production. A restore target carrying one
/// is refused even when `TWO_DATABASE_URL` is unset or names something else.
/// Narrow by design: every name the repo's own scratch flows use (`postgres`,
/// `two_next_*`, `*_test`, `*scratch*`, `*drill*`) stays restorable. Match is
/// exact and case-insensitive; extend only with a code + `docs/backup.md`
/// change so the refusal stays greppable.
const PRODUCTION_DATABASE_NAMES: [&str; 6] = [
    "prod",
    "production",
    "two_prod",
    "two_production",
    "live",
    "two_live",
];

/// What the restore guard can prove about a URL's live-target identity.
#[derive(Debug, PartialEq, Eq)]
enum TargetProof {
    /// Unparsable input or a non-Postgres scheme: the caller falls through
    /// to the existing invalid-URL refusal, which already handles these.
    Unparsed,
    /// Parsable, but the effective target cannot be named exactly the way
    /// the driver and server resolve it: fail closed and refuse.
    Unprovable,
    /// The (host, port, database) the URL actually connects to.
    Known {
        host: String,
        port: u16,
        dbname: String,
    },
}

/// Socket directories compare by path components: the driver builds the socket
/// file as `{dir}/.s.PGSQL.{port}` (`fetch_socket`) and the OS resolves
/// redundant separators, `.` segments, and trailing slashes to the same file,
/// so every spelling of one directory must compare equal. `..` segments pop
/// lexically (over-folding only ever over-refuses a scratch target, never
/// under-refuses a live one). TCP hostnames never reach this function.
fn normalize_socket_dir(value: &str) -> String {
    use std::path::{Component, Path};
    let mut stack: Vec<String> = Vec::new();
    let mut rooted = false;
    for component in Path::new(value).components() {
        match component {
            Component::Prefix(prefix) => {
                stack.push(prefix.as_os_str().to_string_lossy().into_owned());
            }
            Component::RootDir => rooted = true,
            Component::CurDir => {}
            Component::ParentDir => {
                stack.pop();
            }
            Component::Normal(part) => stack.push(part.to_string_lossy().into_owned()),
        }
    }
    let mut normalized = String::new();
    if rooted {
        normalized.push('/');
    }
    normalized.push_str(&stack.join("/"));
    if normalized.is_empty() {
        "/".to_owned()
    } else {
        normalized
    }
}

/// Default port exactly as the driver seeds it: `PGPORT` when it parses as a
/// port, else 5432 (sqlx-postgres `options/mod.rs::new_without_pgpass`).
/// Split from the environment read so tests pin the fallback without
/// mutating process env (which would race parallel tests).
fn default_port_from_env(pgport: Option<&str>) -> u16 {
    pgport.and_then(|v| v.parse().ok()).unwrap_or(5432)
}

/// The restore process's own effective default port: it is the process that
/// connects, so its `PGPORT` is the one that steers a portless URL.
fn default_port() -> u16 {
    default_port_from_env(std::env::var("PGPORT").ok().as_deref())
}

/// Live-target identity for the restore refusal: the (host, port, database)
/// a URL actually connects to, with every spelling variation folded away —
/// or proof that the target cannot be named exactly and must be refused.
///
/// Evasion audit (operator-error threat model; the operator types both URLs):
/// - `postgres://` vs `postgresql://` canonicalized (sqlx accepts both).
/// - Authority host percent-decoded before lowercasing: sqlx decodes a socket
///   host (`postgres://%2Fvar%2Frun%2Fpostgresql/app`), so the encoded and
///   `?host=` spellings of one socket compare equal. A decoded socket path
///   (containing `/`) can never collide with a TCP hostname; DNS is
///   case-insensitive.
/// - Socket directories compare by path components: redundant separators,
///   `.` segments, and trailing slashes all reach the same socket file
///   (`{dir}/.s.PGSQL.{port}`), so every spelling of one directory folds.
/// - A missing port defaults from `PGPORT` exactly as the driver seeds it
///   (unparseable or unset means 5432), so a portless URL cannot evade a
///   live URL on the effective port.
/// - `?host=` / `?hostaddr=`, `?port=`, `?dbname=` folded in (libpq/sqlx let
///   query keys override the authority/path); last value wins, matching
///   option-map collection — except a socket directory (leading `/`) and a
///   plain host never merge: the driver's socket slot is sticky (`host()`
///   never clears `socket()`, `fetch_socket()` prefers it), so a URL naming
///   both spellings is unprovable. Either spelling alone stays comparable.
///   An *empty* `?host=`/`?hostaddr=`/`?dbname=` override is unprovable:
///   sqlx applies it unconditionally (empty host = local socket, empty
///   dbname = the server falls back to the username), so the URL no longer
///   names the target the guard compared.
/// - Userinfo and every other query key ignored, so a different password or
///   `sslmode` cannot evade the comparison.
/// - A missing host (unix-socket URLs such as `postgres:///db?host=...`)
///   compares as the empty string instead of failing open, so query
///   overrides still apply and identical socket targets refuse.
/// - Database is the full path with all leading slashes removed, then
///   percent-decoded — the driver's own rule (sqlx-postgres parses the path
///   with `trim_start_matches('/')`), so `//prod` names `prod` exactly as
///   the connection does. Case preserved (Postgres names are case-sensitive).
/// - An omitted database (empty path and no nonempty `?dbname=`) is
///   unprovable: sqlx seeds it from `PGDATABASE` and Postgres otherwise
///   connects to the database named after the user, so neither the marker
///   list nor the live comparison can be trusted. Name the database
///   explicitly to restore.
/// - Unparsable input or a non-Postgres scheme yields `Unparsed`: the caller
///   falls through to the existing invalid-URL refusal. A parsable URL whose
///   effective target cannot be named exactly yields `Unprovable`: refusing
///   an unnameable restore target is the fail-closed contract.
fn target_identity(raw: &str) -> TargetProof {
    target_identity_with_default_port(raw, default_port())
}

/// [`target_identity`] with an explicit default port, so tests pin `PGPORT`
/// behavior without touching process env.
fn target_identity_with_default_port(raw: &str, default_port: u16) -> TargetProof {
    let url = match url::Url::parse(raw.trim()) {
        Ok(url) => url,
        Err(_) => return TargetProof::Unparsed,
    };
    if url.scheme() != "postgres" && url.scheme() != "postgresql" {
        return TargetProof::Unparsed;
    }
    let authority = percent_encoding::percent_decode_str(url.host_str().unwrap_or_default())
        .decode_utf8_lossy()
        .to_lowercase();
    let mut socket_seen = authority.starts_with('/');
    let mut plain_seen = !socket_seen && !authority.is_empty();
    let mut host = if socket_seen {
        normalize_socket_dir(&authority)
    } else {
        authority
    };
    let mut port = url.port().unwrap_or(default_port);
    let mut dbname = percent_encoding::percent_decode_str(url.path().trim_start_matches('/'))
        .decode_utf8_lossy()
        .into_owned();
    let mut unprovable = false;
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "host" | "hostaddr" | "dbname" if value.is_empty() => {
                unprovable = true;
            }
            "host" | "hostaddr" => {
                let value = value.to_lowercase();
                if value.starts_with('/') {
                    socket_seen = true;
                    host = normalize_socket_dir(&value);
                } else {
                    plain_seen = true;
                    host = value;
                }
            }
            "port" => {
                if let Ok(overridden) = value.parse::<u16>() {
                    port = overridden;
                }
            }
            "dbname" => {
                dbname = value.into_owned();
            }
            _ => {}
        }
    }
    if unprovable || (socket_seen && plain_seen) || dbname.is_empty() {
        return TargetProof::Unprovable;
    }
    TargetProof::Known { host, port, dbname }
}

/// True when `candidate` must be refused as a restore target: it normalizes
/// to the live database, its database name is a production marker, or its
/// effective target cannot be proven different from live (omitted database,
/// empty overrides, socket/host mixes — see `target_identity`). `live` is
/// `TWO_DATABASE_URL`
/// when set; with no live URL only the marker list applies. A live URL that
/// is itself unprovable also refuses: it cannot be proven different from the
/// candidate. Never echoes either input (returns `bool`, not a message).
#[must_use]
pub fn restore_target_is_live(candidate: &str, live: Option<&str>) -> bool {
    let (host, port, dbname) = match target_identity(candidate) {
        TargetProof::Unprovable => return true,
        TargetProof::Unparsed => return false,
        TargetProof::Known { host, port, dbname } => (host, port, dbname),
    };
    if PRODUCTION_DATABASE_NAMES.contains(&dbname.to_lowercase().as_str()) {
        return true;
    }
    let Some(live) = live else {
        return false;
    };
    let (live_host, live_port, live_db) = match target_identity(live) {
        TargetProof::Unprovable => return true,
        TargetProof::Unparsed => return false,
        TargetProof::Known { host, port, dbname } => (host, port, dbname),
    };
    host == live_host && port == live_port && dbname == live_db
}
/// carries the raw file line, which can hold a credential). Re-audit on SQLx
/// upgrades: pinned sqlx-postgres 0.9 logs from `options::pgpass`.
#[cfg(feature = "db")]
fn is_passfile_target(target: &str) -> bool {
    target == "sqlx_postgres::options::pgpass"
}

/// Parse-scoped subscriber: drop only the passfile target, forward everything
/// else to the ambient subscriber. Spans delegate untouched so in-flight
/// instrumentation keeps its identities.
#[cfg(feature = "db")]
struct PassfileSilencer(Dispatch);

#[cfg(feature = "db")]
impl Subscriber for PassfileSilencer {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        !is_passfile_target(metadata.target()) && self.0.enabled(metadata)
    }

    fn new_span(&self, span: &Attributes<'_>) -> Id {
        self.0.new_span(span)
    }

    fn record(&self, span: &Id, values: &Record<'_>) {
        self.0.record(span, values);
    }

    fn record_follows_from(&self, span: &Id, follows: &Id) {
        self.0.record_follows_from(span, follows);
    }

    fn event(&self, event: &Event<'_>) {
        if !is_passfile_target(event.metadata().target()) {
            self.0.event(event);
        }
    }

    fn enter(&self, span: &Id) {
        self.0.enter(span);
    }

    fn exit(&self, span: &Id) {
        self.0.exit(span);
    }

    fn clone_span(&self, id: &Id) -> Id {
        self.0.clone_span(id)
    }

    fn try_close(&self, id: Id) -> bool {
        self.0.try_close(id)
    }
}

#[cfg(any(feature = "db", test))]
fn url_for_sqlx(raw: &str) -> Result<String, &'static str> {
    validate(raw)?;
    let mut url = url::Url::parse(raw).map_err(|_| "invalid database URL")?;
    if !url.query_pairs().any(|(key, _)| key == "channel_binding") {
        return Ok(raw.to_owned());
    }
    let pairs: Vec<_> = url
        .query_pairs()
        .filter(|(key, _)| key != "channel_binding")
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    url.set_query(None);
    if !pairs.is_empty() {
        url.query_pairs_mut().extend_pairs(pairs);
    }
    Ok(url.into())
}

/// Parse a validated database URL into SQLx options.
/// All decoded occurrences of libpq's `channel_binding` are dropped before the
/// driver sees them. This does not enforce channel binding or change SSL mode.
///
/// Unsupported query keys are refused before the driver can WARN-log them.
/// The parse itself runs under a thread-scoped
/// subscriber that drops the driver's passfile target: a malformed passfile
/// line (which can contain a credential) must not reach logs, while a
/// well-formed entry still supplies the password exactly as `FromStr` would.
/// The scope is thread-local and synchronous, so other threads keep the
/// ambient subscriber and no await point can interleave on this thread.
#[cfg(feature = "db")]
pub fn connect_options(url: &str) -> Result<PgConnectOptions, sqlx::Error> {
    let url =
        url_for_sqlx(url).map_err(|message| sqlx::Error::InvalidArgument(message.to_owned()))?;
    let current = tracing::dispatcher::get_default(|dispatch| dispatch.clone());
    tracing::subscriber::with_default(PassfileSilencer(current), || {
        PgConnectOptions::from_str(&url)
            .map_err(|_| sqlx::Error::InvalidArgument("invalid database URL".to_owned()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supported_sqlx_query_keys_remain_accepted() {
        for key in [
            "sslmode",
            "ssl-mode",
            "sslrootcert",
            "ssl-root-cert",
            "ssl-ca",
            "sslcert",
            "ssl-cert",
            "sslkey",
            "ssl-key",
            "statement-cache-capacity",
            "host",
            "hostaddr",
            "port",
            "dbname",
            "user",
            "password",
            "application_name",
            "options",
            "options[statement_timeout]",
        ] {
            assert!(validate(&format!("postgres://agent-testdb/db?{key}=fixture")).is_ok());
        }
    }

    #[test]
    fn planetscale_production_shapes_validate() {
        // Synthetic PlanetScale production shapes: dotted `{role}.{branch_id}`
        // logins on the direct `*.pg.psdb.cloud:5432` and
        // `*.horizon.psdb.cloud:5432` endpoints with allowlisted keys only.
        // Synthetic hostnames: no real cluster id, host or credential.
        for raw in [
            "postgresql://migrator.cnfixture01@psdb-fixture-1.pg.psdb.cloud:5432/postgres?sslmode=require",
            "postgresql://migrator.cnfixture01@psdb-fixture-1.pg.psdb.cloud:5432/postgres?sslmode=verify-full",
            "postgresql://fixture-role.cnfixture02:fixture-password@psdb-fixture-2.pg.psdb.cloud:5432/two_bot?sslmode=require&channel_binding=require",
            "postgresql://migrator.cnfixture01@psdb-fixture-1.horizon.psdb.cloud:5432/postgres?sslmode=require",
            "postgres://fixture-role.cnfixture02:fixture-password@psdb-fixture-2.horizon.psdb.cloud/db?sslmode=verify-ca",
            "postgresql://migrator.cnfixture01@psdb-fixture-1.pg.psdb.cloud:5432/postgres?sslmode=require&options[statement_timeout]=5000",
        ] {
            assert!(validate(raw).is_ok(), "{raw}");
        }
    }

    #[test]
    fn neon_channel_binding_is_removed_without_changing_sslmode() {
        for query in [
            "sslmode=require&channel_binding=require",
            "channel%5Fbinding=fixture-secret&sslmode=require&channel_binding=require",
        ] {
            let raw =
                format!("postgres://fixture:fixture-password@ep-fixture.neon.tech/db?{query}");
            assert!(validate(&raw).is_ok());
            let filtered = url::Url::parse(&url_for_sqlx(&raw).unwrap()).unwrap();
            assert_eq!(filtered.query().unwrap(), "sslmode=require");
            assert_eq!(filtered.password(), Some("fixture-password"));
            #[cfg(feature = "db")]
            assert!(matches!(
                connect_options(&raw).unwrap().get_ssl_mode(),
                sqlx::postgres::PgSslMode::Require
            ));
        }
        assert_eq!(
            url_for_sqlx("postgres://agent-testdb/db?channel_binding=require").unwrap(),
            "postgres://agent-testdb/db"
        );
        let unknown = "postgres://agent-testdb/db?channel_binding=require&api_key=fixture-secret";
        assert!(url_for_sqlx(unknown).is_err());
        #[cfg(feature = "db")]
        {
            let invalid = "postgres://agent-testdb/db?sslmode=invalid&channel_binding=require";
            assert!(connect_options(invalid).is_err());
        }
    }

    #[test]
    fn restore_refuses_live_valued_target_despite_spelling_variants() {
        let live = "postgres://restore-probe:fixture-live@127.0.0.1:1/two_prod";
        // Exact equality, scheme alias, and every spelling that still names
        // the same database refuse.
        for candidate in [
            live.to_owned(),
            "postgresql://restore-probe:fixture-live@127.0.0.1:1/two_prod".to_owned(),
            "postgres://other-user:other-password@127.0.0.1:1/two_prod".to_owned(),
            "postgres://restore-probe:fixture-live@127.0.0.1:1/two_prod?sslmode=require".to_owned(),
            "  postgres://restore-probe:fixture-live@127.0.0.1:1/two_prod  ".to_owned(),
        ] {
            assert!(
                restore_target_is_live(&candidate, Some(live)),
                "{candidate}"
            );
        }
        // A genuinely distinct scratch target proceeds.
        assert!(!restore_target_is_live(
            "postgres://restore-probe:fixture-scratch@127.0.0.1:2/scratch_db",
            Some(live)
        ));
    }

    #[test]
    fn restore_normalizes_host_case_default_port_and_query_overrides() {
        // Host case folds; an explicit default port equals an omitted one.
        assert!(restore_target_is_live(
            "postgres://u:p@DB-HOST:5432/app",
            Some("postgres://u:p@db-host/app")
        ));
        // A different port or database is a different target.
        assert!(!restore_target_is_live(
            "postgres://u:p@db-host:5433/app",
            Some("postgres://u:p@db-host/app")
        ));
        assert!(!restore_target_is_live(
            "postgres://u:p@db-host/app2",
            Some("postgres://u:p@db-host/app")
        ));
        // Database names stay case-sensitive.
        assert!(!restore_target_is_live(
            "postgres://u:p@db-host/APP",
            Some("postgres://u:p@db-host/app")
        ));
        // libpq query overrides cannot smuggle the live target past the
        // comparison: host/dbname/port keys fold into the identity.
        assert!(restore_target_is_live(
            "postgres://u:p@decoy-host/decoy?host=db-host&dbname=app",
            Some("postgres://u:p@db-host/app")
        ));
        assert!(restore_target_is_live(
            "postgres://u:p@db-host:9999/app?port=5432",
            Some("postgres://u:p@db-host/app")
        ));
        // An unparseable port override is ignored, not fatal.
        assert!(!restore_target_is_live(
            "postgres://u:p@db-host:9999/app?port=not-a-port",
            Some("postgres://u:p@db-host/app")
        ));
    }

    #[test]
    fn restore_covers_hostless_socket_urls_and_encoded_names() {
        // Hostless unix-socket URLs must not fail open: the `?host=`
        // override still applies and identical socket targets refuse.
        let live = "postgres:///app?host=/var/run/postgresql";
        assert!(restore_target_is_live(
            "postgres:///app?host=/var/run/postgresql",
            Some(live)
        ));
        assert!(!restore_target_is_live(
            "postgres:///app?host=/other/socket",
            Some(live)
        ));
        // Markers fire on hostless candidates too.
        assert!(restore_target_is_live(
            "postgres:///two_prod?host=/var/run/postgresql",
            None
        ));
        // The path segment is percent-decoded before comparison, matching
        // the `?dbname=` override and the driver's own decoding.
        assert!(restore_target_is_live(
            "postgres://u:p@db-host/two%5Fprod",
            Some("postgres://u:p@db-host/two_prod")
        ));
        assert!(restore_target_is_live("postgres://u:p@h/pr%6Fd", None));
    }

    #[test]
    fn restore_derives_double_slash_databases_like_the_driver() {
        // sqlx parses the path with `trim_start_matches('/')`, so `//prod`
        // names `prod`: the guard derives the database the same way instead
        // of reading only the first (empty) path segment.
        assert!(restore_target_is_live(
            "postgres://u:p@db-host//prod",
            Some("postgres://u:p@db-host/prod")
        ));
        assert!(restore_target_is_live(
            "postgres://u:p@db-host///two_prod",
            Some("postgres://u:p@db-host/two_prod")
        ));
        // Markers fire on double-slash spellings with no live URL either.
        assert!(restore_target_is_live(
            "postgres://u:p@db-host//two_prod",
            None
        ));
        // A genuinely different double-slash database still proceeds.
        assert!(!restore_target_is_live(
            "postgres://u:p@db-host//scratch_db",
            Some("postgres://u:p@db-host/other_db")
        ));
    }

    #[test]
    fn restore_folds_encoded_socket_hosts_both_directions() {
        let encoded = "postgres://%2Fvar%2Frun%2Fpostgresql/app";
        let query = "postgres:///app?host=/var/run/postgresql";
        // Both spellings connect to the same socket and database: each
        // refuses when the other is live.
        assert!(restore_target_is_live(encoded, Some(query)));
        assert!(restore_target_is_live(query, Some(encoded)));
        // A different socket stays a different target.
        assert!(!restore_target_is_live(
            "postgres:///app?host=/other/socket",
            Some(encoded)
        ));
    }

    #[test]
    fn restore_refuses_targets_that_name_no_database() {
        // An omitted database is unprovable: the driver seeds it from
        // `PGDATABASE` and the server otherwise falls back to the username,
        // so the guard refuses instead of comparing an empty name.
        assert!(restore_target_is_live(
            "postgres://prod@db-host",
            Some("postgres://prod@db-host/prod")
        ));
        assert!(restore_target_is_live("postgres://u:p@db-host/", None));
        assert!(restore_target_is_live(
            "postgres://u:p@db-host",
            Some("postgres://u:p@other/scratch_db")
        ));
        // A live URL that names no database cannot be proven different from
        // the candidate either.
        assert!(restore_target_is_live(
            "postgres://u:p@db-host/scratch_db",
            Some("postgres://u:p@db-host")
        ));
    }

    #[test]
    fn restore_refuses_empty_host_and_dbname_overrides() {
        // sqlx applies empty overrides unconditionally while the naive read
        // skips them, so a target carrying one no longer names the compared
        // target: fail closed.
        assert!(restore_target_is_live(
            "postgres://u:p@decoy-host/app?host=",
            Some("postgres://u:p@other/scratch_db")
        ));
        assert!(restore_target_is_live(
            "postgres://u:p@decoy-host/app?hostaddr=",
            Some("postgres://u:p@other/scratch_db")
        ));
        assert!(restore_target_is_live(
            "postgres://u:p@db-host/scratch_db?dbname=",
            Some("postgres://u:p@db-host/postgres")
        ));
        // Empty overrides on the live side are unprovable too.
        assert!(restore_target_is_live(
            "postgres://u:p@db-host/scratch_db",
            Some("postgres://u:p@db-host/scratch_db?dbname=")
        ));
    }

    #[test]
    fn restore_refuses_socket_urls_mixed_with_plain_host_overrides() {
        // The driver's socket slot is sticky: `?host=` with a leading `/`
        // (or a socket authority) calls `socket()`, while a later plain
        // `?host=`/`?hostaddr=` only calls `host()` — and `fetch_socket()`
        // still prefers the slot. A last-wins string cannot name that
        // target, so any socket/plain mix is unprovable.
        let live = "postgres:///app?host=/var/run/postgresql";
        for candidate in [
            // `?host=<socket>` then a plain hostaddr: driver stays on socket.
            "postgres://u:p@localhost/app?host=/var/run/postgresql&hostaddr=127.0.0.1",
            // Socket authority then a plain `?host=`: the slot survives.
            "postgres://%2Fvar%2Frun%2Fpostgresql/app?host=localhost",
            // Socket `?host=` then a plain `?host=`: same sticky slot.
            "postgres:///app?host=/var/run/postgresql&host=localhost",
            // Order-independent: plain first, socket second is still a mix.
            "postgres:///app?host=localhost&host=/var/run/postgresql",
            // A TCP authority with a socket override is a mix too.
            "postgres://u:p@db-host/app?host=/var/run/postgresql",
        ] {
            assert!(restore_target_is_live(candidate, Some(live)), "{candidate}");
            // Unprovable refuses even with no live URL to compare against.
            assert!(restore_target_is_live(candidate, None), "{candidate}");
        }
        // One spelling alone stays comparable: the same socket twice still
        // names the live target.
        assert!(restore_target_is_live(
            "postgres://%2Fvar%2Frun%2Fpostgresql/app?host=/var/run/postgresql",
            Some(live)
        ));
    }

    #[test]
    fn restore_folds_socket_directory_spellings() {
        // The driver builds the socket file as `{dir}/.s.PGSQL.{port}` and
        // the OS resolves redundant separators, `.` segments, and trailing
        // slashes to the same file — so every spelling of one directory
        // refuses when it is the live socket.
        let live = "postgres:///app?host=/var/run/postgresql";
        for candidate in [
            "postgres:///app?host=/var/run/postgresql/",
            "postgres:///app?host=/var//run/postgresql",
            "postgres:///app?host=/var/run/./postgresql",
            "postgres:///app?host=//var/run/postgresql",
            "postgres:///app?host=/var/run/postgresql/../postgresql",
            "postgres://%2Fvar%2Frun%2Fpostgresql%2F/app",
            "postgres://%2Fvar%2F%2Frun%2Fpostgresql/app",
        ] {
            assert!(restore_target_is_live(candidate, Some(live)), "{candidate}");
            assert!(restore_target_is_live(live, Some(candidate)), "{candidate}");
        }
        // A different socket directory stays a different target.
        assert!(!restore_target_is_live(
            "postgres:///app?host=/other/socket/",
            Some(live)
        ));
        assert!(!restore_target_is_live(
            "postgres:///app?host=/var/run/other",
            Some(live)
        ));
    }

    #[test]
    fn restore_defaults_portless_urls_from_pgport_like_the_driver() {
        // The driver seeds the port from PGPORT before URL fields override it
        // (`new_without_pgpass`); the guard takes the same default so a
        // portless URL cannot evade a live URL on the effective port.
        let portless = "postgres://u:p@db-host/app";
        let on_5433 = "postgres://u:p@db-host:5433/app";
        let on_5432 = "postgres://u:p@db-host:5432/app";
        assert_eq!(
            target_identity_with_default_port(portless, 5433),
            target_identity_with_default_port(on_5433, 5433)
        );
        assert_ne!(
            target_identity_with_default_port(portless, 5433),
            target_identity_with_default_port(on_5432, 5433)
        );
        // Explicit ports ignore the default on both sides.
        assert_ne!(
            target_identity_with_default_port(on_5432, 5433),
            target_identity_with_default_port(on_5433, 5433)
        );
        // A `?port=` override still wins over the default.
        assert_ne!(
            target_identity_with_default_port("postgres://u:p@db-host/app?port=5432", 5433),
            target_identity_with_default_port(on_5433, 5433)
        );
        // The default itself mirrors the driver's seed exactly: PGPORT when
        // it parses as a port, else 5432.
        assert_eq!(default_port_from_env(None), 5432);
        assert_eq!(default_port_from_env(Some("5433")), 5433);
        for pgport in ["", "not-a-port", "99999", "-1", "5432x"] {
            assert_eq!(default_port_from_env(Some(pgport)), 5432, "{pgport}");
        }
    }

    #[test]
    fn restore_refuses_production_marked_names_without_a_live_url() {
        for name in [
            "prod",
            "production",
            "two_prod",
            "two_production",
            "live",
            "two_live",
            "PROD",
            "Two_Prod",
        ] {
            let candidate = format!("postgres://u:p@scratch-host:5432/{name}");
            assert!(restore_target_is_live(&candidate, None), "{candidate}");
            // Markers also fire when a live URL is set but differs.
            assert!(
                restore_target_is_live(&candidate, Some("postgres://u:p@other/db")),
                "{candidate}"
            );
        }
        // The repo's own scratch shapes stay restorable without a live URL.
        for name in ["postgres", "two_next_restore_drill", "scratch_db", "db"] {
            let candidate = format!("postgres://u:p@scratch-host:5432/{name}");
            assert!(!restore_target_is_live(&candidate, None), "{candidate}");
        }
        // Near-markers are not markers: exact match only.
        assert!(!restore_target_is_live("postgres://u:p@h/reproduce", None));
    }

    #[test]
    fn restore_comparison_never_panics_and_never_invents_a_refusal() {
        let live = "postgres://u:p@127.0.0.1:1/two_prod";
        for candidate in [
            String::new(),
            " ".to_owned(),
            "not a url".to_owned(),
            "http://127.0.0.1:1/two_prod".to_owned(),
            "postgres://[::1/db".to_owned(),
            "postgres://h:99999999999999999999/db".to_owned(),
        ] {
            assert!(
                !restore_target_is_live(&candidate, Some(live)),
                "{candidate}"
            );
            assert!(!restore_target_is_live(&candidate, None), "{candidate}");
        }
        // An unparsable live URL disables the equality half, not the markers.
        assert!(!restore_target_is_live(
            "postgres://u:p@h/scratch_db",
            Some("not a url")
        ));
        assert!(restore_target_is_live(
            "postgres://u:p@h/two_prod",
            Some("not a url")
        ));
        // Deterministic: repeated comparisons agree, so no hidden state escapes.
        let candidate = "postgresql://u:p@127.0.0.1:1/two_prod?sslmode=require";
        let first = restore_target_is_live(candidate, Some(live));
        assert_eq!(
            restore_target_is_live(candidate, Some(live)),
            first,
            "restore_target_is_live must compare deterministically"
        );
    }

    #[test]
    fn unknown_query_keys_fail_without_echoing_keys_or_values() {
        for query in [
            "api_key=fixture-query-secret&sslmode=invalid",
            "fixture-key-secret=fixture-value-secret",
            "api%5Fkey=fixture-query-secret",
            "options[bad=fixture-query-secret",
        ] {
            let error = validate(&format!("postgres://agent-testdb/db?{query}")).unwrap_err();
            assert_eq!(error, "unsupported database URL parameter");
            assert!(!error.contains("fixture"));
        }
    }

    #[test]
    fn hostile_inputs_never_panic_and_stay_fixed_vocabulary() {
        const INVALID_URL: &str = "invalid database URL";
        const UNSUPPORTED_PARAM: &str = "unsupported database URL parameter";
        let overlong = "a".repeat(70_000);
        let many_unsupported = format!(
            "postgres://agent-testdb/db?{}",
            "x=fixture-secret&".repeat(1_000)
        );
        let cases: Vec<String> = vec![
            String::new(),
            " ".to_owned(),
            "\0".to_owned(),
            "postgres://agent-testdb/db?sslmode=require\r\nInjected: fixture-secret".to_owned(),
            "postgres://agent-testdb/db?api_key=fixture-secret".to_owned(),
            "postgres://agent-testdb/db?fixture-key-secret=fixture-value-secret".to_owned(),
            "postgres://agent-testdb/db?api%5Fkey=fixture-query-secret".to_owned(),
            "postgres://agent-testdb/db?options[bad=fixture-query-secret".to_owned(),
            "postgres://user:fixture-secret@agent-testdb/db?sslmode=require".to_owned(),
            "😀postgres://agent-testdb/db?sslmode=require".to_owned(),
            "postgres://agent-testdb/db?sslmode=\0fixture-secret".to_owned(),
            overlong,
            many_unsupported,
            "http://agent-testdb/db?api_key=fixture-secret".to_owned(),
            "postgres://agent-testdb:99999999999999999999/db".to_owned(),
            "postgres://[::1/db".to_owned(),
            "postgres://agent-testdb/db?sslmode=require%00fixture-secret".to_owned(),
            "postgres://agent-testdb/db?%0d%0a=fixture-secret".to_owned(),
        ];
        for raw in &cases {
            let first = validate(raw);
            // Determinism: repeated parses agree, so no hidden state escapes.
            assert_eq!(
                validate(raw),
                first,
                "validate must parse deterministically"
            );
            match first {
                Ok(()) => {}
                Err(message) => {
                    assert!(
                        message == INVALID_URL || message == UNSUPPORTED_PARAM,
                        "validate escaped a non-fixed error string"
                    );
                    // Failing input is never echoed: sentinel bytes from the
                    // hostile matrix must not appear in the fixed refusal, and
                    // refusals stay single-line.
                    assert!(!message.contains("fixture"));
                    assert!(!message.contains("SENTINEL"));
                    assert!(!message.contains('\r'));
                    assert!(!message.contains('\n'));
                    assert!(!message.contains('\0'));
                }
            }
        }
        // Valid secrets in supported positions still validate: the hold is
        // about error text, not about refusing credentials themselves.
        assert!(validate("postgres://user:fixture-secret@agent-testdb/db?sslmode=require").is_ok());
        // Unsupported refuses before SQLx can WARN-log key and value: the
        // refusal is the fixed pre-parse string, never a driver message.
        assert_eq!(
            validate("postgres://agent-testdb/db?api_key=fixture-secret"),
            Err(UNSUPPORTED_PARAM)
        );
    }

    #[cfg(feature = "db")]
    #[test]
    fn connect_options_hostile_inputs_stay_fixed_vocabulary() {
        const INVALID_URL: &str = "invalid database URL";
        const UNSUPPORTED_PARAM: &str = "unsupported database URL parameter";
        fn message(error: &sqlx::Error) -> &str {
            assert!(
                matches!(error, sqlx::Error::InvalidArgument(_)),
                "connect_options escaped a non-fixed variant: {error:?}"
            );
            match error {
                sqlx::Error::InvalidArgument(message) => message,
                // Assert above already fails the test; return fixed vocabulary
                // to satisfy types without introducing a prod-panic site (the
                // semgrep rule cannot tell inline test helpers apart).
                _ => "invalid database URL",
            }
        }
        let many_unsupported = format!(
            "postgres://agent-testdb/db?{}",
            "x=fixture-secret&".repeat(1_000)
        );
        let cases: Vec<String> = vec![
            String::new(),
            "not a database url".to_owned(),
            "postgres://agent-testdb/db?api_key=fixture-secret&sslmode=require".to_owned(),
            "postgres://agent-testdb/db?fixture-key-secret=fixture-value-secret".to_owned(),
            "postgres://agent-testdb/db?sslmode=require\r\nInjected: fixture-secret".to_owned(),
            "postgres://agent-testdb/db?sslmode=invalid".to_owned(),
            "postgres://agent-testdb/db?sslmode=\0fixture-secret".to_owned(),
            "😀postgres://agent-testdb/db?sslmode=require".to_owned(),
            many_unsupported,
            "postgres://agent-testdb:99999999999999999999/db".to_owned(),
        ];
        for raw in &cases {
            let validated = validate(raw);
            let connected = connect_options(raw);
            // Determinism across the tracing-scoped parse.
            assert_eq!(
                connect_options(raw).is_ok(),
                connected.is_ok(),
                "connect_options must parse deterministically"
            );
            match validated {
                Err(expected) => {
                    // Refusal before SQLx: the pre-parse message propagates
                    // unchanged, so unsupported values never reach the driver.
                    let error =
                        connected.expect_err("validate refusal must refuse connect_options");
                    assert_eq!(message(&error), expected);
                }
                Ok(()) => match connected {
                    Ok(_) => {}
                    Err(error) => {
                        assert_eq!(message(&error), INVALID_URL);
                    }
                },
            }
            if let Err(error) = connect_options(raw) {
                let text = message(&error);
                assert!(
                    text == INVALID_URL || text == UNSUPPORTED_PARAM,
                    "connect_options escaped a non-fixed error string"
                );
                assert!(!text.contains("fixture"));
                assert!(!text.contains("SENTINEL"));
                assert!(!text.contains('\r'));
                assert!(!text.contains('\n'));
            }
        }
        // The driver-value failure stays fixed too: a bad but supported value
        // never echoes the rejected text.
        let error = connect_options("postgres://agent-testdb/db?sslmode=fixture-secret")
            .expect_err("bad sslmode value must refuse");
        assert_eq!(message(&error), INVALID_URL);
        assert!(!message(&error).contains("fixture"));
    }
}
