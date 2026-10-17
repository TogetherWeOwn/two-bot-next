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
            match error {
                sqlx::Error::InvalidArgument(message) => message,
                other => panic!("connect_options escaped a non-fixed variant: {other:?}"),
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
