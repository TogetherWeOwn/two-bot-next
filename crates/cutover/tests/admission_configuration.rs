//! Cutover's runtime authority parser must refuse before DB or Discord I/O.
#[path = "../../core/tests/support/tracing_capture.rs"]
mod tracing_capture;

#[test]
fn admission_bootstrap_query_guard_child() {
    if std::env::var_os("ADMISSION_CUTOVER_PROBE").is_none() {
        return;
    }
    let capture = tracing_capture::Capture::default();
    tracing::subscriber::with_default(capture.clone(), || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let error = two_bot_cutover::RestClient::from_env(
                "fixture-token".to_owned(),
                Some("http://127.0.0.1:1".to_owned()),
            )
            .await
            .unwrap_err();
            assert_eq!(
                error.to_string(),
                "discord send refused: admission authority unavailable"
            );
            tracing::warn!("cutover admission capture remains active");
        });
    });
    let text = capture.text();
    assert!(text.contains("cutover admission capture remains active"));
    assert!(!text.contains("fixture-cutover-query-secret"));
    assert!(!text.contains("ignoring unrecognized connect parameter"));
}

#[test]
fn admission_bootstrap_query_guard_redacts_dependency_logs() {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .env_clear()
        .args(["admission_bootstrap_query_guard_child", "--exact", "--nocapture"])
        .env("ADMISSION_CUTOVER_PROBE", "1")
        .env("TWO_DATABASE_URL", "postgres://fixture:fixture-password@127.0.0.1:1/fixture?api_key=fixture-cutover-query-secret")
        .output().unwrap();
    assert!(
        output.status.success(),
        "isolated cutover admission guard probe failed"
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("running 1 test"));
    for text in [
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    ] {
        assert!(!text.contains("fixture-cutover-query-secret"));
    }
}
