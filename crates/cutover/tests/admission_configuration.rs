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

/// Threat-model F6: a non-TLS Neon URL is refused through the real `from_env`
/// entry (unset `TWO_DATABASE_TLS` means `Required`) with the same redacted
/// authority error, before SQLx parses the URL or opens a socket.
#[test]
fn admission_bootstrap_tls_refusal_child() {
    if std::env::var_os("ADMISSION_TLS_PROBE").is_none() {
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
            assert!(!error.to_string().contains("fixture"));
            assert!(!format!("{error:?}").contains("fixture"));
            tracing::warn!("cutover TLS admission capture remains active");
        });
    });
    let text = capture.text();
    assert!(text.contains("cutover TLS admission capture remains active"));
    assert!(!text.contains("fixture"));
    assert!(!text.contains("ignoring unrecognized connect parameter"));
}

#[test]
fn admission_bootstrap_tls_refusal_redacts_dependency_logs() {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .env_clear()
        .args(["admission_bootstrap_tls_refusal_child", "--exact", "--nocapture"])
        .env("ADMISSION_TLS_PROBE", "1")
        .env("TWO_DATABASE_URL", "postgres://fixture-user:fixture-db-password@ep-fixture-host.us-east-2.aws.neon.tech/fixture-db?sslmode=disable")
        .output().unwrap();
    assert!(
        output.status.success(),
        "isolated cutover TLS admission guard probe failed"
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("running 1 test"));
    for text in [
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    ] {
        assert!(!text.contains("fixture"));
    }
}

/// Threat-model F6 entry proof that DISCRIMINATES the fence (child half).
/// The parent points `TWO_DATABASE_URL` at its own loopback spy listener
/// with a plaintext mode under the default `Required` policy. Fenced code
/// refuses before any socket, so this child exits fast with the redacted
/// error; unfenced code dials the spy (then hangs in startup against the
/// silent listener), which the parent observes.
#[test]
fn admission_bootstrap_tls_spy_child() {
    if std::env::var_os("ADMISSION_SPY_PROBE").is_none() {
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
            assert!(!error.to_string().contains("fixture"));
            assert!(!format!("{error:?}").contains("fixture"));
            tracing::warn!("cutover TLS spy capture remains active");
        });
    });
    let text = capture.text();
    assert!(text.contains("cutover TLS spy capture remains active"));
    assert!(!text.contains("fixture"));
    assert!(!text.contains("ignoring unrecognized connect parameter"));
}

/// Parent half of the discriminating entry proof: bind a loopback listener
/// that is never accepted (the kernel still completes dials into the
/// backlog, so any dial attempt stays observable), run the child above
/// against it, then assert no dial arrived. A mutant that restores raw
/// `connect_options` in `from_env` dials the spy — or hangs in startup, and
/// the bounded wait turns that hang into a failure instead of a stuck suite.
#[test]
fn admission_bootstrap_tls_spy_refuses_before_any_socket() {
    use std::io::{ErrorKind, Read as _};
    use std::process::Stdio;
    use std::time::{Duration, Instant};
    let spy = std::net::TcpListener::bind("127.0.0.1:0").expect("spy listener");
    let port = spy.local_addr().expect("spy addr").port();
    let url = format!(
        "postgres://fixture-user:fixture-db-password@127.0.0.1:{port}/fixture-db?sslmode=disable"
    );
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .env_clear()
        .args([
            "admission_bootstrap_tls_spy_child",
            "--exact",
            "--nocapture",
        ])
        .env("ADMISSION_SPY_PROBE", "1")
        .env("TWO_DATABASE_URL", &url)
        // No TWO_DATABASE_TLS: unset means Required, so plaintext refuses.
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn spy probe");
    let deadline = Instant::now() + Duration::from_secs(30);
    let (status, stdout, stderr) = loop {
        match child.try_wait().expect("probe wait") {
            Some(status) => {
                // The child has exited, so both pipes are at EOF: drain them
                // directly instead of another wait on the reaped process.
                let mut stdout = Vec::new();
                let mut stderr = Vec::new();
                child
                    .stdout
                    .take()
                    .expect("probe stdout")
                    .read_to_end(&mut stdout)
                    .expect("drain probe stdout");
                child
                    .stderr
                    .take()
                    .expect("probe stderr")
                    .read_to_end(&mut stderr)
                    .expect("drain probe stderr");
                break (status, stdout, stderr);
            }
            None if Instant::now() > deadline => {
                child.kill().ok();
                let _ = child.wait();
                panic!("TLS spy probe timed out: from_env dialed instead of refusing");
            }
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    };
    assert!(status.success(), "isolated cutover TLS spy probe failed");
    assert!(String::from_utf8_lossy(&stdout).contains("running 1 test"));
    for text in [
        String::from_utf8_lossy(&stdout),
        String::from_utf8_lossy(&stderr),
    ] {
        assert!(!text.contains("fixture"));
    }
    spy.set_nonblocking(true).expect("spy nonblocking");
    match spy.accept() {
        Err(error) if error.kind() == ErrorKind::WouldBlock => {}
        Ok(_) => panic!("TLS fence dialed the database instead of refusing"),
        Err(error) => panic!("spy accept failed: {error}"),
    }
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
