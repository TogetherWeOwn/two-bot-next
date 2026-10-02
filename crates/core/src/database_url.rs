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
                | "options"
        ) || (key.starts_with("options[") && key.ends_with(']'));
        if !supported {
            return Err("unsupported database URL parameter");
        }
    }
    Ok(())
}

/// Target emitting the passfile diagnostics (`Malformed line in pgpass file`
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

/// Validate and parse a database URL into SQLx options.
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
    validate(url).map_err(|message| sqlx::Error::InvalidArgument(message.to_owned()))?;
    let current = tracing::dispatcher::get_default(|dispatch| dispatch.clone());
    tracing::subscriber::with_default(PassfileSilencer(current), || {
        PgConnectOptions::from_str(url)
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
}
