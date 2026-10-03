//! Threat-model F6 refusals for the gateway store pool (TOG-12208).
//!
//! Mirrors `crates/cutover/tests/secret_connection.rs`: a remote
//! `sslmode=disable` URL (and the other refusal cases) fail with a fixed
//! string before SQLx parses the URL or opens a socket, with no URL part in
//! the error or the logs. Hermetic: every case is refused, so no connection
//! is attempted and no database is needed.
#[path = "../../core/tests/support/tracing_capture.rs"]
mod tracing_capture;

use two_bot_core::database_tls::TlsPolicy;

/// Threat-model F6 refusals happen before SQLx parses the URL or opens a
/// socket: the error is a fixed string and no URL part reaches any log level.
#[test]
fn tls_policy_refusals_never_echo_urls_or_reach_logs() {
    let cases = [
        (
            "postgres://fixture-user:fixture-db-password@ep-fixture-host.us-east-2.aws.neon.tech/fixture-db?sslmode=disable",
            TlsPolicy::Required,
            "database sslmode does not require TLS",
        ),
        (
            "postgres://fixture-user:fixture-db-password@ep-fixture-host.us-east-2.aws.neon.tech/fixture-db",
            TlsPolicy::Required,
            "database URL must set sslmode under the required TLS policy",
        ),
        (
            "postgres://fixture-user:fixture-db-password@ep-fixture-host.us-east-2.aws.neon.tech/fixture-db?sslmode=prefer",
            TlsPolicy::LocalOnly,
            "remote database host is refused under the local-only TLS policy",
        ),
        (
            "postgres://fixture-user:fixture-db-password@fixture-host/fixture-db?sslmode=verify-full",
            TlsPolicy::Required,
            "local database host is refused under the required TLS policy",
        ),
        (
            "postgres://fixture-user:fixture-db-password@fixture-host/fixture-db?sslmode=fixture-mode",
            TlsPolicy::LocalOnly,
            "unsupported database sslmode",
        ),
    ];
    let capture = tracing_capture::Capture::default();
    tracing::subscriber::with_default(capture.clone(), || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            for (url, policy, expected) in cases {
                let error = two_bot_store::connect_pool_with_tls(url, policy)
                    .await
                    .unwrap_err();
                assert_eq!(error.to_string(), expected);
                for shown in [format!("{error}"), format!("{error:?}")] {
                    assert!(!shown.contains("fixture"), "TLS refusal echoed the URL");
                }
            }
            tracing::warn!("capture remains active");
        });
    });
    let text = capture.text();
    assert!(text.contains("capture remains active"));
    assert!(!text.contains("fixture"), "TLS refusal reached logs");
}
