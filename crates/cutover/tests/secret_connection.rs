//! Malformed fixture URLs fail before opening a socket.
#[path = "../../core/tests/support/tracing_capture.rs"]
mod tracing_capture;

/// Child-process probe: a malformed passfile entry must not reach WARN logs.
/// Runs only when `CUTOVER_PGPASS_PROBE` is set (see the parent test below);
/// a no-op in the normal suite. Opens no connection and prints no credential.
#[test]
fn pgpass_probe_redacts_malformed_entry_child() {
    if std::env::var("CUTOVER_PGPASS_PROBE").is_err() {
        return;
    }
    let capture = tracing_capture::Capture::default();
    tracing::subscriber::with_default(capture.clone(), || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            // Passwordless URL with no URL/env password: the driver consults
            // the passfile. The parent points PGPASSFILE at a synthetic
            // malformed fixture, never a real credential or file.
            let error = two_bot_cutover::connect(
                "postgres://fixture-user@agent-testdb/db?sslmode=disable",
                1,
                true,
            )
            .await
            .unwrap_err();
            // Parse succeeded and only the connection failed: the passfile
            // lookup ran (no URL/env password), so a vacuous pass is excluded.
            assert_eq!(error.to_string(), "database connection failed");
            for shown in [format!("{error}"), format!("{error:?}")] {
                assert!(!shown.contains("fixture-pgpass-secret"));
            }
            tracing::warn!("capture remains active");
        });
    });
    let text = capture.text();
    assert!(text.contains("capture remains active"));
    assert!(
        !text.contains("fixture-pgpass-secret"),
        "malformed passfile entry reached WARN logs"
    );
    assert!(!text.contains("Malformed line in pgpass file"));
}

/// Parent: re-run only the probe above in a child test-binary process with a
/// synthetic malformed `PGPASSFILE`, an empty `HOME` (no real `~/.pgpass`),
/// and no `PGPASSWORD`. A separate process avoids process-global environment
/// races with the parallel suite. Credential-free: the fixture holds a
/// synthetic sentinel and no connection can succeed in either process.
#[test]
fn malformed_passfile_entry_never_reaches_warn_logs() {
    if std::env::var("CUTOVER_PGPASS_PROBE").is_ok() {
        return; // child run: covered by the probe test above
    }
    let dir = std::env::temp_dir().join(format!(
        "two-bot-pgpass-probe-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("probe scratch dir");
    // First fields match the probe URL, then a truncated tail: sqlx logs the
    // whole raw line at WARN while parsing, before any sanitized error.
    let passfile = dir.join("pgpass");
    std::fs::write(&passfile, "agent-testdb:5432:db:fixture-pgpass-secret\n")
        .expect("write synthetic passfile fixture");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&passfile, std::fs::Permissions::from_mode(0o600))
            .expect("owner-only passfile fixture");
    }
    let home = dir.join("home");
    std::fs::create_dir_all(&home).expect("empty probe HOME");
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "pgpass_probe_redacts_malformed_entry_child",
            "--exact",
            "--nocapture",
        ])
        .env("CUTOVER_PGPASS_PROBE", "1")
        .env("PGPASSFILE", &passfile)
        .env("HOME", &home)
        .env_remove("PGPASSWORD")
        .env_remove("PGOPTIONS")
        // Pin the passfile match fields so the malformed line is evaluated,
        // not skipped on a host/port/user mismatch.
        .env_remove("PGHOST")
        .env_remove("PGHOSTADDR")
        .env_remove("PGPORT")
        .env_remove("PGUSER")
        .env_remove("PGDATABASE")
        .output()
        .expect("run isolated passfile probe");
    std::fs::remove_dir_all(&dir).ok();
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "passfile probe failed: {stderr}");
    assert!(
        !stderr.contains("fixture-pgpass-secret"),
        "probe stderr leaked"
    );
    assert!(
        !stdout.contains("fixture-pgpass-secret"),
        "probe stdout leaked"
    );
}

#[test]
fn unsupported_query_secrets_never_reach_sqlx_warn_logs() {
    let capture = tracing_capture::Capture::default();
    tracing::subscriber::with_default(capture.clone(), || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let error = two_bot_cutover::connect(
                "postgres://fixture-user:fixture-password@agent-testdb/db?api_key=fixture-query-secret&sslmode=invalid",
                1,
                true,
            )
            .await
            .unwrap_err();
            for shown in [format!("{error}"), format!("{error:?}")] {
                assert!(!shown.contains("fixture-query-secret"));
            }
            tracing::warn!("capture remains active");
        });
    });
    let text = capture.text();
    assert!(text.contains("capture remains active"));
    assert!(!text.contains("fixture-query-secret"));
    assert!(!text.contains("ignoring unrecognized connect parameter"));
}
#[test]
fn neon_channel_binding_never_reaches_sqlx_warn_logs() {
    let capture = tracing_capture::Capture::default();
    tracing::subscriber::with_default(capture.clone(), || {
        for query in [
            "sslmode=require&channel_binding=require",
            "channel%5Fbinding=fixture-binding-secret&sslmode=require&channel_binding=fixture-repeated-secret",
        ] {
            // Parse only: this synthetic Neon hostname is never contacted.
            let url =
                format!("postgres://fixture-user:fixture-password@ep-fixture.neon.tech/db?{query}");
            let options = two_bot_core::database_url::connect_options(&url).unwrap();
            assert_eq!(options.get_ssl_mode(), sqlx::postgres::PgSslMode::Require);
        }
        tracing::warn!("capture remains active");
    });
    let text = capture.text();
    assert!(text.contains("capture remains active"));
    for forbidden in [
        "fixture-binding-secret",
        "fixture-repeated-secret",
        "channel_binding",
        "ignoring unrecognized connect parameter",
    ] {
        assert!(!text.contains(forbidden));
    }
}

#[tokio::test]
async fn connection_errors_and_their_source_chains_never_echo_urls() {
    for url in [
        "postgres://fixture-user:fixture-db-password@agent-testdb:fixture-invalid-port/db",
        "postgres://fixture-user:fixture-db-password@agent-testdb/db?sslmode=fixture-invalid-mode",
        "invalid://fixture-user:fixture-db-password@agent-testdb/db",
    ] {
        let error = two_bot_cutover::connect(url, 1, true).await.unwrap_err();
        let mut current: Option<&(dyn std::error::Error + 'static)> = Some(&error);
        while let Some(error) = current {
            for output in [format!("{error}"), format!("{error:?}")] {
                for secret in [
                    url,
                    "fixture-user",
                    "fixture-db-password",
                    "fixture-invalid-port",
                    "fixture-invalid-mode",
                ] {
                    assert!(
                        !output.contains(secret),
                        "connection diagnostic leaked fixture credential"
                    );
                }
            }
            current = error.source();
        }
    }
}
