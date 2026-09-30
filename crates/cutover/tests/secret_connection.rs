//! Malformed fixture URLs fail before opening a socket.
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
