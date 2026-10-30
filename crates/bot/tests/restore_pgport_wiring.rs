//! PGPORT wiring pin for the restore live-target guard.
//!
//! `target_identity` defaults a portless URL from `PGPORT` exactly as the
//! driver seeds it — but that environment read lives in `default_port()`,
//! behind the `restore_target_is_live -> target_identity` production path.
//! Unit tests pin only the pure seams, so this test pins the wiring itself:
//! the real `two-bot restore` binary runs in a child process with
//! `PGPORT=5433`, a live URL on `:5433`, and a portless candidate. The guard
//! must refuse (exit 2, stable code) before any connection attempt.
//!
//! Offline: the refusal precedes file parsing and pool-open, so the ports
//! are never contacted. Child-only env (`env_clear` + explicit vars), so no
//! process-global mutation can race another test.

use std::path::PathBuf;
use std::process::Output;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::process::Command;

const LIVE_TARGET_CODE: &str = "code=restore_live_target_refused";
const LIVE_SECRET_SENTINEL: &str = "pgport-live-pw-8f3a-wiring";
const CANDIDATE_SECRET_SENTINEL: &str = "pgport-cand-pw-1c7e-wiring";

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::var_os("PAPERCLIP_RUN_SCRATCH_DIR")
            .or_else(|| std::env::var_os("PAPERCLIP_SCRATCH_DIR"))
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let dir = root.join(format!(
            "restore-pgport-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn restore_portless_candidate_refuses_when_pgport_steers_to_live_port() {
    let dir = Scratch::new("refusal");
    let file = dir.0.join("dump.ndjson.gz");
    std::fs::write(&file, b"never read: the refusal precedes file parsing").unwrap();
    let live = format!("postgres://restore-probe:{LIVE_SECRET_SENTINEL}@127.0.0.1:5433/app");
    let candidate = format!("postgres://restore-probe:{CANDIDATE_SECRET_SENTINEL}@127.0.0.1/app");

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_two-bot"));
    // Child-only env isolation: no inherited database, proxy, or uploader,
    // and no process-global env mutation can race another test.
    cmd.env_clear()
        .kill_on_drop(true)
        .current_dir(&dir.0)
        .arg("restore")
        .arg(&file)
        .arg("--force")
        .env("TWO_DATABASE_URL", &live)
        .env("TWO_RESTORE_URL", &candidate)
        .env("PGPORT", "5433");
    // Under cargo-llvm-cov the child is instrumented: forward the harness
    // redirect (it carries `%p`/`%m`, so each child keeps its own profile).
    if let Some(profile) = std::env::var_os("LLVM_PROFILE_FILE") {
        cmd.env("LLVM_PROFILE_FILE", profile);
    }
    let output: Output = tokio::time::timeout(Duration::from_secs(15), cmd.output())
        .await
        .expect("restore CLI timed out")
        .expect("CLI failed to start");
    let stdout = String::from_utf8(output.stdout.clone()).unwrap();
    let stderr = String::from_utf8(output.stderr.clone()).unwrap();

    assert_eq!(
        output.status.code(),
        Some(2),
        "stdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stderr.contains(LIVE_TARGET_CODE),
        "stdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stderr.contains("refus"),
        "stdout: {stdout}\nstderr: {stderr}"
    );
    for secret in [
        LIVE_SECRET_SENTINEL,
        CANDIDATE_SECRET_SENTINEL,
        live.as_str(),
        candidate.as_str(),
    ] {
        assert!(
            !stdout.contains(secret) && !stderr.contains(secret),
            "secret echoed. stdout: {stdout}\nstderr: {stderr}"
        );
    }
    assert!(
        !format!("{stdout}{stderr}").contains("RESTORE VERIFIED"),
        "stdout: {stdout}\nstderr: {stderr}"
    );
}
