//! Env-cleared child probes avoid racing process-global bootstrap configuration.
#[path = "../../core/tests/support/tracing_capture.rs"]
mod tracing_capture;

pub fn capture_probe(future: impl std::future::Future<Output = ()>) {
    let capture = tracing_capture::Capture::default();
    tracing::subscriber::with_default(capture.clone(), || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(future);
        tracing::warn!("bootstrap admission capture remains active");
    });
    let text = capture.text();
    assert!(text.contains("bootstrap admission capture remains active"));
    assert!(!text.contains("fixture-bootstrap-secret"));
    assert!(!text.contains("ignoring unrecognized connect parameter"));
}

pub fn run_probe(name: &str) {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .env_clear()
        .args([name, "--exact", "--nocapture"])
        .env("ADMISSION_BOOTSTRAP_PROBE", "1")
        .env("TWO_DATABASE_URL", "postgres://fixture:fixture-password@127.0.0.1:1/fixture?api_key=fixture-bootstrap-secret")
        .env("TWO_GUILD_CONFIG_OFFLINE_TEST", "1")
        .env("GUILD_CONFIG_API_BASE", "http://127.0.0.1:1/api/v10")
        .env("GUILD_CONFIG_CDN_BASE", "http://127.0.0.1:1")
        .output().unwrap();
    assert!(
        output.status.success(),
        "isolated bootstrap guard probe failed"
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("running 1 test"));
    for text in [
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    ] {
        assert!(!text.contains("fixture-bootstrap-secret"));
    }
}
