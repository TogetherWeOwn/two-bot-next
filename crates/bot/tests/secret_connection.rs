//! Capture the real CLI diagnostic for a fixture URL rejected before networking.
use std::process::Command;

#[test]
fn backup_connection_failure_does_not_echo_database_url() {
    let base = std::env::var_os("PAPERCLIP_RUN_SCRATCH_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let dir = base.join(format!("redaction-backup-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let url = "postgres://fixture-user:fixture-db-password@agent-testdb:fixture-invalid-port/db";
    let output = Command::new(env!("CARGO_BIN_EXE_two-bot"))
        .arg("backup")
        .env_clear()
        .env("TWO_DATABASE_URL", url)
        .env("TWO_BACKUP_DIR", &dir)
        .output()
        .unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("cannot connect"),
        "unexpected diagnostic: {stderr}"
    );
    for secret in [
        url,
        "fixture-user",
        "fixture-db-password",
        "fixture-invalid-port",
    ] {
        assert!(!stdout.contains(secret));
        assert!(!stderr.contains(secret));
    }
}
