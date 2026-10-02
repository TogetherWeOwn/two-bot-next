//! Fence database URLs to authenticated TLS before SQLx sees them
//! (threat-model F6). See `docs/database-tls.md`.
//!
//! [`enforce`] is pure: it reads only the URL and the policy, never the
//! environment, DNS or a socket. Errors are static strings, so no URL, host or
//! credential can reach an error chain or a log through this module.
//!
//! Under [`TlsPolicy::Required`] the effective SQLx mode is always
//! `verify-full` ([`apply`]). In pinned sqlx-postgres 0.9 with rustls,
//! `require` accepts any certificate and `verify-ca` skips the hostname check,
//! and SQLx does not implement libpq's `channel_binding`. Neon's default
//! `sslmode=require&channel_binding=require` URL is therefore accepted and then
//! verified in full against the bundled webpki roots. Re-audit on SQLx upgrades.

#[cfg(feature = "db")]
use sqlx::postgres::{PgConnectOptions, PgSslMode};
use std::net::IpAddr;

/// Process setting that selects the policy. Unset means [`TlsPolicy::Required`].
pub const POLICY_SETTING: &str = "TWO_DATABASE_TLS";

/// Which database hosts a process may reach, and how.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TlsPolicy {
    /// Remote hosts only, with an explicit TLS sslmode; connections verify the
    /// certificate chain and hostname. The deployed default.
    Required,
    /// Loopback, single-label CI service hosts and Unix sockets only, with any
    /// sslmode. Tests and CI opt in; it never downgrades a remote host.
    LocalOnly,
}

impl TlsPolicy {
    /// Parse the [`POLICY_SETTING`] value. `None` (unset) is `Required`; any
    /// value other than `required` or `local-only` is refused.
    pub fn from_setting(value: Option<&str>) -> Result<Self, &'static str> {
        match value {
            None | Some("required") => Ok(Self::Required),
            Some("local-only") => Ok(Self::LocalOnly),
            Some(_) => Err("invalid TWO_DATABASE_TLS setting"),
        }
    }
}

const INVALID_URL: &str = "invalid database URL";
const MISSING_HOST: &str = "database URL host is required";
const LOCAL_REFUSED: &str = "local database host is refused under the required TLS policy";
const REMOTE_REFUSED: &str = "remote database host is refused under the local-only TLS policy";
const MISSING_SSLMODE: &str = "database URL must set sslmode under the required TLS policy";
const WEAK_SSLMODE: &str = "database sslmode does not require TLS";
const UNKNOWN_SSLMODE: &str = "unsupported database sslmode";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HostClass {
    Remote,
    Loopback,
    CiService,
    Socket,
}

/// Refuse a URL that the policy does not allow.
///
/// Every host the driver could use counts: the URL authority and each `host`
/// or `hostaddr` query value. A URL with none is refused because SQLx would
/// fall back to `PGHOST` or a local default. Every `sslmode`/`ssl-mode`
/// occurrence counts too, matched case-insensitively like SQLx.
pub fn enforce(raw: &str, policy: TlsPolicy) -> Result<(), &'static str> {
    let url = url::Url::parse(raw).map_err(|_| INVALID_URL)?;
    let mut hosts = Vec::new();
    match url.host() {
        Some(url::Host::Domain("")) => {}
        Some(url::Host::Domain(host)) => hosts.push(classify_authority(host)),
        Some(url::Host::Ipv4(ip)) => hosts.push(classify_ip(ip.into())),
        Some(url::Host::Ipv6(ip)) => hosts.push(classify_ip(ip.into())),
        None => {}
    }
    let mut modes = Vec::new();
    for (key, value) in url.query_pairs() {
        match &*key {
            "host" | "hostaddr" if value.is_empty() => return Err(MISSING_HOST),
            "host" if value.starts_with('/') => hosts.push(HostClass::Socket),
            "host" | "hostaddr" => hosts.push(classify_name(&value)),
            "sslmode" | "ssl-mode" => modes.push(value.to_ascii_lowercase()),
            _ => {}
        }
    }
    if hosts.is_empty() {
        return Err(MISSING_HOST);
    }
    for host in hosts {
        match (policy, host) {
            (TlsPolicy::Required, HostClass::Remote) => {}
            (TlsPolicy::Required, _) => return Err(LOCAL_REFUSED),
            (TlsPolicy::LocalOnly, HostClass::Remote) => return Err(REMOTE_REFUSED),
            (TlsPolicy::LocalOnly, _) => {}
        }
    }
    if policy == TlsPolicy::Required && modes.is_empty() {
        return Err(MISSING_SSLMODE);
    }
    for mode in modes {
        match mode.as_str() {
            "require" | "verify-ca" | "verify-full" => {}
            "disable" | "allow" | "prefer" if policy == TlsPolicy::LocalOnly => {}
            "disable" | "allow" | "prefer" => return Err(WEAK_SSLMODE),
            _ => return Err(UNKNOWN_SSLMODE),
        }
    }
    Ok(())
}

/// Set the effective SQLx mode for a URL that passed [`enforce`]: `verify-full`
/// under `Required`, whatever the URL spelled; unchanged under `LocalOnly`.
#[cfg(feature = "db")]
pub fn apply(options: PgConnectOptions, policy: TlsPolicy) -> PgConnectOptions {
    match policy {
        TlsPolicy::Required => options.ssl_mode(PgSslMode::VerifyFull),
        TlsPolicy::LocalOnly => options,
    }
}

/// SQLx treats an authority whose first decoded byte is `/` as a socket path.
fn classify_authority(host: &str) -> HostClass {
    let lower = host.to_ascii_lowercase();
    if lower.starts_with("%2f") {
        HostClass::Socket
    } else {
        classify_name(host)
    }
}

fn classify_name(host: &str) -> HostClass {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return classify_ip(ip);
    }
    if host.eq_ignore_ascii_case("localhost") {
        return HostClass::Loopback;
    }
    // One DNS label starting with a letter (e.g. `agent-testdb`): a container
    // network service name. Digit-led labels are excluded because resolvers
    // read forms such as `2130706433` or `0x7f000001` as IPv4 addresses.
    let single_label = host.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
        && host.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        && !host.ends_with('-');
    if single_label {
        HostClass::CiService
    } else {
        HostClass::Remote
    }
}

fn classify_ip(ip: IpAddr) -> HostClass {
    let loopback = match ip {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => {
            v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
    };
    if loopback {
        HostClass::Loopback
    } else {
        HostClass::Remote
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REQUIRED: TlsPolicy = TlsPolicy::Required;
    const LOCAL_ONLY: TlsPolicy = TlsPolicy::LocalOnly;

    /// Every SQLx sslmode value, absence, an alias, a case variant, Neon's
    /// default query and an unknown value.
    const MODES: [&str; 11] = [
        "",
        "sslmode=disable",
        "sslmode=allow",
        "sslmode=prefer",
        "sslmode=require",
        "sslmode=verify-ca",
        "sslmode=verify-full",
        "ssl-mode=disable",
        "sslmode=VERIFY-FULL",
        "sslmode=require&channel_binding=require",
        "sslmode=fixture-unknown-mode",
    ];

    /// URL prefixes per host class; queries are appended after `?`.
    const REMOTE: [&str; 5] = [
        "postgres://fixture-user:fixture-password@ep-fixture-123.us-east-2.aws.neon.tech/neondb",
        "postgresql://fixture-user:fixture-password@ep-fixture-123-pooler.eu-central-1.aws.neon.tech:5432/neondb",
        "postgres://fixture-user:fixture-password@203.0.113.7/db",
        "postgres://fixture-user:fixture-password@[2001:db8::7]/db",
        "postgres://fixture-user:fixture-password@agent-testdb./db",
    ];
    const LOCAL: [&str; 9] = [
        "postgres://agent_test:@localhost:5432/agent_test",
        "postgres://agent_test:@LOCALHOST/agent_test",
        "postgres://agent_test:@127.0.0.1:5432/agent_test",
        "postgres://agent_test:@127.8.9.10/agent_test",
        "postgres://agent_test:@[::1]:5432/agent_test",
        "postgres://agent_test:@[::ffff:127.0.0.1]/agent_test",
        "postgres://agent_test:@agent-testdb:5432/agent_test",
        "postgres://agent_test:@postgres/agent_test",
        "postgres://agent_test@%2Fvar%2Frun%2Fpostgresql/agent_test",
    ];

    fn url(prefix: &str, query: &str) -> String {
        if query.is_empty() {
            prefix.to_owned()
        } else {
            format!("{prefix}?{query}")
        }
    }

    /// Expected outcome under `Required` for a remote host, by mode index.
    const REQUIRED_REMOTE: [Result<(), &str>; 11] = [
        Err(MISSING_SSLMODE),
        Err(WEAK_SSLMODE),
        Err(WEAK_SSLMODE),
        Err(WEAK_SSLMODE),
        Ok(()),
        Ok(()),
        Ok(()),
        Err(WEAK_SSLMODE),
        Ok(()),
        Ok(()),
        Err(UNKNOWN_SSLMODE),
    ];

    /// Expected outcome under `LocalOnly` for a local host, by mode index.
    const LOCAL_ONLY_LOCAL: [Result<(), &str>; 11] = [
        Ok(()),
        Ok(()),
        Ok(()),
        Ok(()),
        Ok(()),
        Ok(()),
        Ok(()),
        Ok(()),
        Ok(()),
        Ok(()),
        Err(UNKNOWN_SSLMODE),
    ];

    #[test]
    fn every_sslmode_policy_and_host_class() {
        for (index, query) in MODES.iter().enumerate() {
            for prefix in REMOTE {
                let raw = url(prefix, query);
                assert_eq!(enforce(&raw, REQUIRED), REQUIRED_REMOTE[index], "{raw}");
                assert_eq!(enforce(&raw, LOCAL_ONLY), Err(REMOTE_REFUSED), "{raw}");
            }
            for prefix in LOCAL {
                let raw = url(prefix, query);
                assert_eq!(enforce(&raw, REQUIRED), Err(LOCAL_REFUSED), "{raw}");
                assert_eq!(enforce(&raw, LOCAL_ONLY), LOCAL_ONLY_LOCAL[index], "{raw}");
            }
        }
    }

    #[test]
    fn neon_default_url_passes_required() {
        let neon = "postgresql://fixture-user:fixture-password@ep-fixture-123.us-east-2.aws.neon.tech/neondb?sslmode=require&channel_binding=require";
        assert_eq!(enforce(neon, REQUIRED), Ok(()));
        assert_eq!(enforce(neon, LOCAL_ONLY), Err(REMOTE_REFUSED));
    }

    #[test]
    fn every_driver_host_and_sslmode_occurrence_counts() {
        for (raw, required, local_only) in [
            // A query host overrides the authority in SQLx; both must pass.
            (
                "postgres://u:p@agent-testdb/db?host=ep-x.aws.neon.tech&sslmode=require",
                Err(LOCAL_REFUSED),
                Err(REMOTE_REFUSED),
            ),
            (
                "postgres://u:p@ep-x.aws.neon.tech/db?host=localhost&sslmode=require",
                Err(LOCAL_REFUSED),
                Err(REMOTE_REFUSED),
            ),
            (
                "postgres://u:p@ep-x.aws.neon.tech/db?host=%2Ftmp&sslmode=require",
                Err(LOCAL_REFUSED),
                Err(REMOTE_REFUSED),
            ),
            (
                "postgres://u:p@ep-x.aws.neon.tech/db?hostaddr=127.0.0.1&sslmode=require",
                Err(LOCAL_REFUSED),
                Err(REMOTE_REFUSED),
            ),
            (
                "postgres://u:p@ep-x.aws.neon.tech/db?hostaddr=203.0.113.7&sslmode=verify-full",
                Ok(()),
                Err(REMOTE_REFUSED),
            ),
            (
                "postgres:///agent_test?host=/var/run/postgresql&user=agent_test",
                Err(LOCAL_REFUSED),
                Ok(()),
            ),
            // SQLx keeps the last sslmode; any weak occurrence is refused.
            (
                "postgres://u:p@ep-x.aws.neon.tech/db?sslmode=require&sslmode=disable",
                Err(WEAK_SSLMODE),
                Err(REMOTE_REFUSED),
            ),
            (
                "postgres://u:p@ep-x.aws.neon.tech/db?sslmode=disable&sslmode=verify-full",
                Err(WEAK_SSLMODE),
                Err(REMOTE_REFUSED),
            ),
            // Percent-encoded keys decode exactly as SQLx decodes them.
            (
                "postgres://u:p@ep-x.aws.neon.tech/db?sslmode=require&ssl%6Dode=prefer",
                Err(WEAK_SSLMODE),
                Err(REMOTE_REFUSED),
            ),
            // No host at all would fall back to PGHOST or a local default.
            (
                "postgres:///db?sslmode=require",
                Err(MISSING_HOST),
                Err(MISSING_HOST),
            ),
            (
                "postgres://u:p@ep-x.aws.neon.tech/db?host=&sslmode=require",
                Err(MISSING_HOST),
                Err(MISSING_HOST),
            ),
            // Digit-led single labels can be numeric IPv4 forms: never local.
            (
                "postgres://u:p@2130706433/db",
                Err(MISSING_SSLMODE),
                Err(REMOTE_REFUSED),
            ),
            (
                "postgres://u:p@0x7f000001/db",
                Err(MISSING_SSLMODE),
                Err(REMOTE_REFUSED),
            ),
            (
                "postgres://u:p@agent-testdb-/db",
                Err(MISSING_SSLMODE),
                Err(REMOTE_REFUSED),
            ),
            (
                "postgres://u:p@%6Cocalhost/db",
                Err(MISSING_SSLMODE),
                Err(REMOTE_REFUSED),
            ),
            ("not a url", Err(INVALID_URL), Err(INVALID_URL)),
        ] {
            assert_eq!(enforce(raw, REQUIRED), required, "{raw}");
            assert_eq!(enforce(raw, LOCAL_ONLY), local_only, "{raw}");
        }
        // Credentials with an empty authority host do not parse as a URL
        // (WHATWG host-missing); either way it is refused.
        for policy in [REQUIRED, LOCAL_ONLY] {
            let refused = enforce("postgres://u:p@/db?sslmode=require", policy);
            assert!(
                matches!(refused, Err(INVALID_URL | MISSING_HOST)),
                "{refused:?}"
            );
        }
    }

    #[test]
    fn errors_never_echo_url_parts() {
        let secrets = [
            "fixture-user",
            "fixture-password",
            "ep-fixture-123",
            "neon.tech",
            "203.0.113.7",
            "agent-testdb",
            "fixture-unknown-mode",
        ];
        for query in MODES {
            for prefix in REMOTE.iter().chain(LOCAL.iter()) {
                for policy in [REQUIRED, LOCAL_ONLY] {
                    if let Err(error) = enforce(&url(prefix, query), policy) {
                        for secret in secrets {
                            assert!(!error.contains(secret), "{error}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn setting_defaults_to_required_and_refuses_unknown_values() {
        assert_eq!(TlsPolicy::from_setting(None), Ok(REQUIRED));
        assert_eq!(TlsPolicy::from_setting(Some("required")), Ok(REQUIRED));
        assert_eq!(TlsPolicy::from_setting(Some("local-only")), Ok(LOCAL_ONLY));
        for value in [
            "",
            "Local-Only",
            "local_only",
            "disable",
            "off",
            "fixture-secret",
        ] {
            let error = TlsPolicy::from_setting(Some(value)).unwrap_err();
            assert_eq!(error, "invalid TWO_DATABASE_TLS setting");
            assert!(!error.contains("fixture-secret"));
        }
    }

    #[cfg(feature = "db")]
    #[test]
    fn required_always_verifies_full_and_local_only_keeps_the_url_mode() {
        use crate::database_url::{connect_options, validate};
        let neon = "postgresql://fixture-user:fixture-password@ep-fixture-123.us-east-2.aws.neon.tech/neondb";
        for mode in ["require", "verify-ca", "verify-full", "REQUIRE"] {
            let raw = format!("{neon}?sslmode={mode}");
            assert_eq!(enforce(&raw, REQUIRED), Ok(()));
            let options = apply(connect_options(&raw).unwrap(), REQUIRED);
            assert!(
                matches!(options.get_ssl_mode(), PgSslMode::VerifyFull),
                "{mode}"
            );
        }
        // Neon's default URL: refused by the key allowlist until PR #156 lets
        // `channel_binding` through; afterwards it must verify in full.
        let neon_default = format!("{neon}?sslmode=require&channel_binding=require");
        assert_eq!(enforce(&neon_default, REQUIRED), Ok(()));
        if validate(&neon_default).is_ok() {
            let options = apply(connect_options(&neon_default).unwrap(), REQUIRED);
            assert!(matches!(options.get_ssl_mode(), PgSslMode::VerifyFull));
        }
        let local = "postgres://agent_test:@agent-testdb:5432/agent_test?sslmode=disable";
        let options = apply(connect_options(local).unwrap(), LOCAL_ONLY);
        assert!(matches!(options.get_ssl_mode(), PgSslMode::Disable));
    }
}
