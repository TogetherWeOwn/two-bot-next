//! Admission options parsing is offline, guarded, and safe even for lazy pools.
#![cfg(feature = "db")]

#[path = "support/tracing_capture.rs"]
mod tracing_capture;

#[test]
fn admission_options_reject_unsupported_queries_before_dependency_logging() {
    let capture = tracing_capture::Capture::default();
    tracing::subscriber::with_default(capture.clone(), || {
        for query in [
            "api_key=fixture-query-secret",
            "api%5Fkey=fixture-query-secret",
            "fixture-query-key=fixture-query-secret&sslmode=invalid",
        ] {
            let url = format!("postgres://fixture:fixture-password@127.0.0.1:1/fixture?{query}");
            // No separate validate call: the options API itself owns the guard.
            let error = two_bot_core::database_url::connect_options(&url).unwrap_err();
            assert_eq!(error.to_string(), "unsupported database URL parameter");
            let mut source: Option<&(dyn std::error::Error + 'static)> = Some(&error);
            while let Some(error) = source {
                for diagnostic in [format!("{error}"), format!("{error:?}")] {
                    assert!(!diagnostic.contains("fixture"));
                }
                source = error.source();
            }
        }
        tracing::warn!("admission capture remains active");
    });
    let text = capture.text();
    assert!(text.contains("admission capture remains active"));
    assert!(!text.contains("fixture-query"));
    assert!(!text.contains("ignoring unrecognized connect parameter"));
}

#[test]
fn admission_passfile_and_lazy_pool_probe_child() {
    if std::env::var_os("ADMISSION_OPTIONS_PROBE").is_none() {
        return;
    }
    let capture = tracing_capture::Capture::default();
    tracing::subscriber::with_default(capture.clone(), || {
        let options = two_bot_core::database_url::connect_options(
            "postgres://fixture-user@127.0.0.1:1/fixture?sslmode=disable",
        )
        .unwrap();
        // Only synthetic credentials exist in this env-cleared child. Prove a
        // well-formed entry still supplies the password; never print options.
        assert!(format!("{options:?}").contains("password: Some(\"fixture-valid-passfile\")"));
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let pool = sqlx::postgres::PgPoolOptions::new()
                .max_connections(2)
                .connect_lazy_with(options);
            assert_eq!(pool.size(), 0, "lazy startup must not open a connection");
            pool.close().await;
        });
        tracing::warn!("admission passfile capture remains active");
    });
    let text = capture.text();
    assert!(text.contains("admission passfile capture remains active"));
    assert!(!text.contains("fixture-malformed-passfile"));
    assert!(!text.contains("fixture-valid-passfile"));
    assert!(!text.contains("Malformed line in pgpass file"));
}

#[test]
fn admission_passfile_diagnostics_are_suppressed_without_changing_credentials() {
    let dir = std::env::temp_dir().join(format!(
        "two-bot-admission-options-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    ));
    std::fs::create_dir_all(dir.join("home")).unwrap();
    let passfile = dir.join("pgpass");
    std::fs::write(&passfile,
        "127.0.0.1:1:fixture:fixture-malformed-passfile\n127.0.0.1:1:fixture:fixture-user:fixture-valid-passfile\n",
    ).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&passfile, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .env_clear()
        .args([
            "admission_passfile_and_lazy_pool_probe_child",
            "--exact",
            "--nocapture",
        ])
        .env("ADMISSION_OPTIONS_PROBE", "1")
        .env("PGPASSFILE", &passfile)
        .env("HOME", dir.join("home"))
        .output()
        .unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
    assert!(
        output.status.success(),
        "isolated admission options probe failed"
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("running 1 test"));
    for text in [
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    ] {
        assert!(!text.contains("fixture-malformed-passfile"));
        assert!(!text.contains("fixture-valid-passfile"));
    }
}
