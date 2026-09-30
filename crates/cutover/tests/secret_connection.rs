//! Malformed fixture URLs fail before opening a socket.
#[path = "../../core/tests/support/tracing_capture.rs"]
mod tracing_capture;

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
