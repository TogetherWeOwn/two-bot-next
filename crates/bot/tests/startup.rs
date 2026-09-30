//! Offline process fixture: rejected configuration cannot park a healthy bot.
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn configured_database_initialization_failure_exits_nonzero_without_logging_url() {
    for url in [
        "not-postgres://fixture-secret",
        "postgresql://[fixture-secret",
    ] {
        let mut child = Command::new(env!("CARGO_BIN_EXE_two-bot"))
            .env_clear()
            .env("DATABASE_URL", url)
            .env("LISTEN_ADDR", "127.0.0.1:0")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("configured DB failure parked instead of exiting");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let output = child.wait_with_output().unwrap();
        assert_eq!(output.status.code(), Some(1));
        let logs = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(logs.contains("database initialization failed"));
        assert!(!logs.contains("fixture-secret"));
    }
}
