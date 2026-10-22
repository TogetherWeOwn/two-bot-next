//! Protected acceptance: guarded restore targets and landed-only reporting.
//!
//! FROZEN HANDOFF FILE. The paired builder consumes the branch head carrying
//! this file and must keep it byte-identical: no edits, no renames, no
//! ignored tests, no weakening an assertion. Test corrections go back to the
//! test author; the builder never edits this file.
//!
//! Scope: `two-bot restore` must refuse a target that normalizes to the live
//! database (even with `--force`, without echoing the URL or its secrets),
//! `two-bot guild-config-restore --apply` must print per-operation completion
//! only as each operation lands and must name landed vs pending operations on
//! a mid-apply failure, and `--help` must document the refusal and the codes.
//!
//! Stable-code vocabulary: `code=<snake_case>` on the failing command's
//! stderr, extending the backup/upload/snapshot vocabulary (no `restore_*`
//! or `guild_restore_*` code exists there, so these two names are fresh):
//! - `code=restore_live_target_refused`: `TWO_RESTORE_URL` normalizes to the
//!   live database. Exit 2, before any connection attempt.
//! - `code=guild_restore_apply_failed`: a guild-config apply failed partway.
//!   Exit 1, with landed vs pending operations named. Exit 0 plans and fully
//!   landed applies never carry it.
//!
//! GREEN contract (every fix shape this file accepts):
//! - The refusal message wording is the builder's choice, except it is on
//!   stderr, exits 2, carries the exact `code=restore_live_target_refused`
//!   token, and never contains the target URL or any of its secrets.
//! - Normalization must cover at least exact equality and the
//!   `postgres://` / `postgresql://` scheme alias. Broader normalization
//!   (default ports, trailing slashes, prod-marker host lists) is welcome as
//!   long as the scratch-target guard below still passes.
//! - The landed/pending report format is the builder's choice, except one
//!   line contains `landed` and the landed op's label, another (or the same)
//!   line contains `pending` and the pending op's label (lowercase keywords),
//!   and the report carries `code=guild_restore_apply_failed`.
//! - `DID <op>` prints only for operations whose write already succeeded, on
//!   stdout (today's stream). A plan listing before the apply must not use
//!   the `DID` prefix. The landed/pending failure report may use either
//!   stream; on total failure its landed account reports none/empty (the
//!   no-`DID` plus failed-write assertions prove nothing landed).
//! - `--help` (shared usage text) keeps its current head and exit 0, gains
//!   one line containing both `live` and `target`, and names both codes.
//!
//! RED tests (fail on the pre-fix tree): the five `acceptance_*` tests.
//! Guards (pass before and after): scratch-target proceed, both dry-run
//! previews, plan-mode listing. Every test is offline: loopback HTTP stubs
//! and closed-port database URLs only; no database contact, no live Discord.

use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Request, State};
use axum::http::{Method, StatusCode};
use axum::{Json, Router};
use serde_json::{json, Map, Value};
use tokio::process::Command;
use two_bot_core::backup::dump_file;
use two_bot_core::backup::guild_config::{self, STAGING_BOT_APPLICATION_ID, TWO_STAGING_GUILD_ID};

const BOT_ROLE: &str = "100000000000000001";
const TARGET_ROLE: &str = "100000000000000002";
const LIVE_TARGET_CODE: &str = "code=restore_live_target_refused";
const APPLY_FAILED_CODE: &str = "code=guild_restore_apply_failed";
/// Password embedded in the live-valued URLs below. Its presence in any
/// child output proves a secret leak, so every restore test asserts absence.
const LIVE_SECRET_SENTINEL: &str = "live-sentinel-pw-7c2e-acpt";
/// A distinct password for the scratch target, so the proceed-guard also
/// proves the non-refusal path leaks nothing.
const SCRATCH_SECRET_SENTINEL: &str = "scratch-sentinel-pw-3b9d-acpt";

/// A live-database URL value. Port 1 is privileged and never a listener, so
/// the proof is the child's deterministic output, never a live connection
/// (same closed-port probe as `backup_cli_help.rs`).
fn live_url(scheme: &str) -> String {
    format!("{scheme}://restore-probe:{LIVE_SECRET_SENTINEL}@127.0.0.1:1/two_prod")
}

fn scratch_url() -> String {
    format!("postgres://restore-probe:{SCRATCH_SECRET_SENTINEL}@127.0.0.1:2/scratch_db")
}

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::var_os("PAPERCLIP_RUN_SCRATCH_DIR")
            .or_else(|| std::env::var_os("PAPERCLIP_SCRATCH_DIR"))
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let dir = root.join(format!(
            "guarded-restore-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(dir.join("credentials")).unwrap();
        Self(dir)
    }

    fn credential(&self) -> PathBuf {
        self.0.join("credentials/discord_staging_token")
    }

    fn write_token(&self) {
        std::fs::write(self.credential(), format!("{}\n", fake_token())).unwrap();
    }

    fn snapshot(&self, source: Map<String, Value>) -> PathBuf {
        let path = self.0.join("source.json");
        let sealed = guild_config::seal_snapshot(source);
        std::fs::write(&path, serde_json::to_vec(&sealed).unwrap()).unwrap();
        path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Generate an unusable token from the public application ID, never real secrets.
fn fake_token() -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::new();
    for chunk in STAGING_BOT_APPLICATION_ID.as_bytes().chunks(3) {
        let bits = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for index in 0..(chunk.len() + 1) {
            encoded.push(ALPHABET[((bits >> (18 - index * 6)) & 63) as usize] as char);
        }
    }
    let token = format!("{encoded}.loopback-test.not-a-secret");
    assert!(guild_config::check_staging_token(&token).is_ok());
    token
}

fn fixture(permissions: u64, bot_position: i64) -> Map<String, Value> {
    json!({
        "version": 1,
        "applicationId": STAGING_BOT_APPLICATION_ID,
        "guildId": TWO_STAGING_GUILD_ID,
        "guild": {"id": TWO_STAGING_GUILD_ID, "owner_id": "100000000000000099", "name": "Loopback guild"},
        "roles": [
            {"id": TWO_STAGING_GUILD_ID, "name": "@everyone", "position": 0, "permissions": "0", "managed": false},
            {"id": BOT_ROLE, "name": "Owen managed role", "position": bot_position, "permissions": permissions.to_string(), "managed": true},
            {"id": TARGET_ROLE, "name": "Member", "position": 5, "permissions": "0", "managed": false}
        ],
        "channels": [],
        "emojis": []
    })
    .as_object()
    .unwrap()
    .clone()
}

/// Snapshot whose plan is exactly two PATCH ops: `patch role Member` (the
/// live role lacks `color`) then `restore guild settings` (name differs).
/// Positions match, so no role-position op appears.
fn two_op_snapshot() -> Map<String, Value> {
    let mut source = fixture(8, 10);
    source["guild"]["name"] = json!("Restored name");
    for role in source["roles"].as_array_mut().unwrap() {
        if role["id"] == json!(TARGET_ROLE) {
            role.as_object_mut()
                .unwrap()
                .insert("color".to_owned(), json!(11259375));
        }
    }
    source
}

/// Snapshot whose plan is exactly one op: `restore guild settings`.
fn one_op_snapshot() -> Map<String, Value> {
    let mut source = fixture(8, 10);
    source["guild"]["name"] = json!("Restored name");
    source
}

struct FakeState {
    current: Map<String, Value>,
    calls: Mutex<Vec<(Method, String)>>,
    /// Writes before this count succeed; later writes fail. `1` lands the
    /// first op then fails the second; `0` fails the very first write.
    succeed_writes: usize,
    writes_done: Mutex<usize>,
}

async fn fake_discord(
    State(state): State<Arc<FakeState>>,
    request: Request,
) -> (StatusCode, Json<Value>) {
    // Never echo the authorization header, even when authentication fails.
    let expected = format!("Bot {}", fake_token());
    if request
        .headers()
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        != Some(expected.as_str())
    {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"message": "unexpected credential"})),
        );
    }
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    state
        .calls
        .lock()
        .unwrap()
        .push((method.clone(), path.clone()));
    if method != Method::GET {
        let mut done = state.writes_done.lock().unwrap();
        *done += 1;
        if *done > state.succeed_writes {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": "loopback induced failure"})),
            );
        }
        // Discord role PATCH success returns the role object; an empty
        // object is a malformed receipt, not a landed write.
        return (StatusCode::OK, Json(json!({"id": TARGET_ROLE})));
    }
    let guild_path = format!("/guilds/{TWO_STAGING_GUILD_ID}");
    let body = match path.as_str() {
        "/users/@me" => json!({"id": STAGING_BOT_APPLICATION_ID}),
        "/users/@me/guilds" => json!([{"id": TWO_STAGING_GUILD_ID}]),
        p if p == guild_path => state.current["guild"].clone(),
        p if p == format!("{guild_path}/roles") => state.current["roles"].clone(),
        p if p == format!("{guild_path}/channels") => state.current["channels"].clone(),
        p if p == format!("{guild_path}/emojis") => state.current["emojis"].clone(),
        p if p == format!("{guild_path}/members/{STAGING_BOT_APPLICATION_ID}") => {
            json!({"roles": [BOT_ROLE]})
        }
        _ => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"message": "unexpected route"})),
            )
        }
    };
    (StatusCode::OK, Json(body))
}

struct FakeDiscord {
    base: String,
    state: Arc<FakeState>,
    task: tokio::task::JoinHandle<()>,
}

impl FakeDiscord {
    async fn start(current: Map<String, Value>, succeed_writes: usize) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(FakeState {
            current,
            calls: Mutex::new(Vec::new()),
            succeed_writes,
            writes_done: Mutex::new(0),
        });
        let app = Router::new()
            .fallback(fake_discord)
            .with_state(Arc::clone(&state));
        // Source: https://docs.rs/axum/0.8/axum/fn.serve.html
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { base, state, task }
    }

    fn writes(&self) -> Vec<(Method, String)> {
        self.state
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(m, _)| *m != Method::GET)
            .cloned()
            .collect()
    }
}

impl Drop for FakeDiscord {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn cli(scratch: &Scratch, fake: &FakeDiscord, subcommand: &str) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_two-bot"));
    // Child-only env isolation: no real token, proxy, database, or uploader is
    // inherited, and no process-global env mutation can race another test.
    // Source: https://docs.rs/tokio/1/tokio/process/struct.Command.html#method.env_clear
    cmd.env_clear()
        .kill_on_drop(true)
        .current_dir(&scratch.0)
        .arg(subcommand)
        .env("CREDENTIALS_DIRECTORY", scratch.0.join("credentials"))
        .env("DISCORD_STAGING_GUILD_ID", TWO_STAGING_GUILD_ID)
        .env("GUILD_CONFIG_API_BASE", &fake.base)
        .env("GUILD_CONFIG_CDN_BASE", &fake.base)
        .env("TWO_GUILD_CONFIG_OFFLINE_TEST", "1")
        .env("TWO_GUILD_CONFIG_BACKUP_DIR", scratch.0.join("captured"))
        // Restore applies perform no upload; this local no-op performs no
        // network call and inherits only this explicitly scrubbed environment.
        .env("TWO_GUILD_CONFIG_UPLOAD_CMD", "/bin/true");
    forward_coverage(&mut cmd);
    cmd
}

/// Cleared-env database-restore CLI: only the two database URLs are set.
fn restore_cli(scratch: &Scratch, database_url: &str, restore_url: &str) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_two-bot"));
    cmd.env_clear()
        .kill_on_drop(true)
        .current_dir(&scratch.0)
        .env("TWO_DATABASE_URL", database_url)
        .env("TWO_RESTORE_URL", restore_url);
    forward_coverage(&mut cmd);
    cmd
}

fn forward_coverage(cmd: &mut Command) {
    // Under cargo-llvm-cov the child is instrumented and writes its profile
    // to LLVM_PROFILE_FILE. `env_clear` would drop that redirect, so forward
    // the harness redirect (it carries `%p`/`%m`, so each child still gets
    // its own profile); the cleared app env stays cleared.
    if let Some(profile) = std::env::var_os("LLVM_PROFILE_FILE") {
        cmd.env("LLVM_PROFILE_FILE", profile);
    }
}

async fn run(mut cmd: Command) -> (Output, String, String) {
    let output = tokio::time::timeout(Duration::from_secs(15), cmd.output())
        .await
        .expect("loopback CLI timed out")
        .expect("CLI failed to start");
    let stdout = String::from_utf8(output.stdout.clone()).unwrap();
    let stderr = String::from_utf8(output.stderr.clone()).unwrap();
    (output, stdout, stderr)
}

fn combined(stdout: &str, stderr: &str) -> String {
    format!("{stdout}{stderr}")
}

/// A structurally valid archive that dry-run verifies: the legacy v3 native
/// fixture (the same file `backup_dump_publication` proves VERIFIED).
fn valid_archive(dir: &Scratch) -> PathBuf {
    let mut encoder = dump_file::new_encoder();
    for line in include_str!("../../core/tests/fixtures/legacy-v3-native.ndjson").lines() {
        dump_file::write_line(&mut encoder, &serde_json::from_str(line).unwrap()).unwrap();
    }
    let path = dir.0.join("valid.ndjson.gz");
    std::fs::write(&path, dump_file::finish_gzip(encoder).unwrap()).unwrap();
    path
}

/// RED: a prod-valued `TWO_RESTORE_URL` (equal to the live database) is
/// refused with `--force`, exit 2, a stable code, and no secret echo.
/// Fails today because `cmd_restore` never compares the target value: it
/// proceeds to pool-open and exits 1 on the closed port.
#[tokio::test]
async fn acceptance_restore_refuses_live_valued_target_even_with_force() {
    let dir = Scratch::new("live-refusal");
    let file = dir.0.join("dump.ndjson.gz");
    std::fs::write(&file, b"never read: the refusal precedes file parsing").unwrap();
    let live = live_url("postgres");

    let mut cmd = restore_cli(&dir, &live, &live);
    cmd.arg("restore").arg(&file).arg("--force");
    let (output, stdout, stderr) = run(cmd).await;

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
    for secret in [LIVE_SECRET_SENTINEL, live.as_str()] {
        assert!(
            !stdout.contains(secret) && !stderr.contains(secret),
            "secret echoed. stdout: {stdout}\nstderr: {stderr}"
        );
    }
    assert!(
        !combined(&stdout, &stderr).contains("RESTORE VERIFIED"),
        "stdout: {stdout}\nstderr: {stderr}"
    );
}

/// RED: the live comparison normalizes the `postgres://` vs `postgresql://`
/// scheme alias (sqlx accepts both for the same server). Fails today for the
/// same reason as the exact-equality refusal: no value comparison at all.
#[tokio::test]
async fn acceptance_restore_refuses_scheme_alias_of_live_database() {
    let dir = Scratch::new("alias-refusal");
    let file = dir.0.join("dump.ndjson.gz");
    std::fs::write(&file, b"never read: the refusal precedes file parsing").unwrap();
    let live = live_url("postgres");
    let alias = live_url("postgresql");

    let mut cmd = restore_cli(&dir, &live, &alias);
    cmd.arg("restore").arg(&file).arg("--force");
    let (output, stdout, stderr) = run(cmd).await;

    assert_eq!(
        output.status.code(),
        Some(2),
        "stdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stderr.contains(LIVE_TARGET_CODE),
        "stdout: {stdout}\nstderr: {stderr}"
    );
    for secret in [LIVE_SECRET_SENTINEL, alias.as_str()] {
        assert!(
            !stdout.contains(secret) && !stderr.contains(secret),
            "secret echoed. stdout: {stdout}\nstderr: {stderr}"
        );
    }
}

/// Guard: a genuinely distinct scratch target proceeds past the live-target
/// guard into the existing flow (closed port: exit 1, no refusal code).
/// Catches a builder over-refusal that blocks legitimate scratch restores.
#[tokio::test]
async fn restore_scratch_target_proceeds_past_live_target_guard() {
    let dir = Scratch::new("scratch-proceeds");
    let file = dir.0.join("dump.ndjson.gz");
    std::fs::write(&file, b"never read: the closed port fails first").unwrap();
    let live = live_url("postgres");
    let scratch = scratch_url();

    let mut cmd = restore_cli(&dir, &live, &scratch);
    cmd.arg("restore").arg(&file).arg("--force");
    let (output, stdout, stderr) = run(cmd).await;

    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        !combined(&stdout, &stderr).contains(LIVE_TARGET_CODE),
        "scratch target wrongly refused. stdout: {stdout}\nstderr: {stderr}"
    );
    for secret in [
        LIVE_SECRET_SENTINEL,
        SCRATCH_SECRET_SENTINEL,
        live.as_str(),
        scratch.as_str(),
    ] {
        assert!(
            !stdout.contains(secret) && !stderr.contains(secret),
            "secret echoed. stdout: {stdout}\nstderr: {stderr}"
        );
    }
}

/// Guard: `--dry-run` writes nothing, so a live-valued target must not turn
/// the preview into a refusal. The builder's live check must sit behind the
/// dry-run branch (or exempt it), not in front of it.
#[tokio::test]
async fn restore_dry_run_previews_with_live_valued_target() {
    let dir = Scratch::new("dry-run-live");
    let file = valid_archive(&dir);
    let live = live_url("postgres");

    let mut cmd = restore_cli(&dir, &live, &live);
    cmd.arg("restore").arg(&file).arg("--dry-run");
    let (output, stdout, stderr) = run(cmd).await;

    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains("DRY RUN VERIFIED"),
        "stdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        !combined(&stdout, &stderr).contains(LIVE_TARGET_CODE),
        "dry run wrongly refused. stdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        !stdout.contains(LIVE_SECRET_SENTINEL) && !stderr.contains(LIVE_SECRET_SENTINEL),
        "secret echoed. stdout: {stdout}\nstderr: {stderr}"
    );
}

/// Guard: the ordinary scratch dry-run preview keeps working.
#[tokio::test]
async fn restore_dry_run_previews_with_scratch_target() {
    let dir = Scratch::new("dry-run-scratch");
    let file = valid_archive(&dir);

    let mut cmd = restore_cli(&dir, &live_url("postgres"), &scratch_url());
    cmd.arg("restore").arg(&file).arg("--dry-run");
    let (output, stdout, stderr) = run(cmd).await;

    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains("DRY RUN VERIFIED"),
        "stdout: {stdout}\nstderr: {stderr}"
    );
}

/// RED: `--help` documents the live-target refusal and both new codes.
/// Fails today because usage text names neither code nor the live-target
/// rule. The usage head and exit 0 are pinned so the addition cannot break
/// the existing help contract.
#[tokio::test]
async fn acceptance_help_documents_live_target_refusal_and_codes() {
    let dir = Scratch::new("help-codes");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_two-bot"));
    cmd.args(["restore", "--help"])
        .env_clear()
        .kill_on_drop(true)
        .current_dir(&dir.0);
    forward_coverage(&mut cmd);
    let (output, stdout, stderr) = run(cmd).await;

    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.starts_with("two-bot operator commands\n"),
        "stdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains(LIVE_TARGET_CODE) && stdout.contains(APPLY_FAILED_CODE),
        "stdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout
            .lines()
            .any(|line| line.contains("live") && line.contains("target")),
        "stdout: {stdout}\nstderr: {stderr}"
    );
}

/// Plan-mode op labels in order. Panics loudly (not a silent skip) when the
/// planner stops emitting exactly `expected` ops, so fixture drift fails
/// visibly instead of asserting against the wrong ops.
async fn plan_labels(
    scratch: &Scratch,
    fake: &FakeDiscord,
    source: &Path,
    expected: usize,
) -> Vec<String> {
    let mut cmd = cli(scratch, fake, "guild-config-restore");
    cmd.arg("--snapshot").arg(source);
    let (output, stdout, stderr) = run(cmd).await;
    let text = combined(&stdout, &stderr);
    assert_eq!(output.status.code(), Some(0), "{text}");
    let labels: Vec<String> = stdout
        .lines()
        .filter_map(|line| line.strip_prefix("WOULD ").map(str::to_owned))
        .collect();
    assert_eq!(
        labels.len(),
        expected,
        "planner emitted {} ops, expected {expected}. {text}",
        labels.len()
    );
    labels
}

/// RED: with a two-op plan where the second write fails, only the landed op
/// prints `DID`, and the failure names the landed op vs the pending op with
/// a stable code. Fails today because every `DID` prints before the apply
/// and the failure is a bare error with no landed/pending account.
#[tokio::test]
async fn acceptance_guild_restore_mid_apply_failure_names_landed_and_pending() {
    let scratch = Scratch::new("landed-pending");
    scratch.write_token();
    let source = scratch.snapshot(two_op_snapshot());
    let fake = FakeDiscord::start(fixture(8, 10), 1).await;

    let labels = plan_labels(&scratch, &fake, &source, 2).await;
    let (landed_op, pending_op) = (labels[0].clone(), labels[1].clone());

    let mut cmd = cli(&scratch, &fake, "guild-config-restore");
    cmd.arg("--snapshot")
        .arg(&source)
        .arg("--confirm-staging-guild")
        .arg("--apply");
    let (output, stdout, stderr) = run(cmd).await;
    let text = combined(&stdout, &stderr);

    assert_eq!(output.status.code(), Some(1), "{text}");
    assert!(!text.contains(&fake_token()), "CLI logged a token. {text}");
    // Exactly two Discord writes happened: the first op landed, the second
    // op's write failed. Apply order is roles before settings.
    let writes = fake.writes();
    assert_eq!(writes.len(), 2, "{text}");
    assert!(
        writes[0].1.ends_with(&format!("/roles/{TARGET_ROLE}")),
        "first write was not the role op: {:?}. {text}",
        writes[0]
    );
    assert!(
        writes[1]
            .1
            .ends_with(&format!("/guilds/{TWO_STAGING_GUILD_ID}")),
        "second write was not the settings op: {:?}. {text}",
        writes[1]
    );
    // Completion prints only for the op that landed.
    assert!(
        stdout
            .lines()
            .any(|line| line == format!("DID {landed_op}")),
        "landed op has no DID line. {text}"
    );
    assert!(
        !stdout
            .lines()
            .any(|line| line == format!("DID {pending_op}")),
        "pending op wrongly claims DID. {text}"
    );
    // The failure names landed vs pending with a stable code.
    assert!(
        text.lines()
            .any(|line| line.contains("landed") && line.contains(&landed_op)),
        "failure names no landed op. {text}"
    );
    assert!(
        text.lines()
            .any(|line| line.contains("pending") && line.contains(&pending_op)),
        "failure names no pending op. {text}"
    );
    assert!(text.contains(APPLY_FAILED_CODE), "{text}");
}

/// RED: with a one-op plan where the write fails, nothing prints `DID`, and
/// the failure still carries the landed (empty) vs pending account plus the
/// stable code. Fails today for the same reason: pre-printed `DID`, bare
/// error.
#[tokio::test]
async fn acceptance_guild_restore_single_op_failure_reports_empty_landed() {
    let scratch = Scratch::new("empty-landed");
    scratch.write_token();
    let source = scratch.snapshot(one_op_snapshot());
    let fake = FakeDiscord::start(fixture(8, 10), 0).await;

    let labels = plan_labels(&scratch, &fake, &source, 1).await;
    let pending_op = labels[0].clone();

    let mut cmd = cli(&scratch, &fake, "guild-config-restore");
    cmd.arg("--snapshot")
        .arg(&source)
        .arg("--confirm-staging-guild")
        .arg("--apply");
    let (output, stdout, stderr) = run(cmd).await;
    let text = combined(&stdout, &stderr);

    assert_eq!(output.status.code(), Some(1), "{text}");
    assert!(!text.contains(&fake_token()), "CLI logged a token. {text}");
    assert_eq!(fake.writes().len(), 1, "{text}");
    assert!(
        !stdout.lines().any(|line| line.starts_with("DID ")),
        "failed op wrongly claims DID. {text}"
    );
    // The landed account exists but claims nothing: with the only write
    // failed and no `DID` line, a `landed` line can only report none/empty,
    // on its own line or shared with the pending account.
    assert!(
        text.lines().any(|line| line.contains("landed")),
        "failure names no landed account. {text}"
    );
    assert!(
        text.lines()
            .any(|line| line.contains("pending") && line.contains(&pending_op)),
        "failure names no pending op. {text}"
    );
    assert!(text.contains(APPLY_FAILED_CODE), "{text}");
}

/// Guard: plan mode still lists every op with `WOULD` and writes nothing.
#[tokio::test]
async fn guild_restore_plan_mode_lists_operations_without_writing() {
    let scratch = Scratch::new("plan-mode");
    scratch.write_token();
    let source = scratch.snapshot(two_op_snapshot());
    // Even a fake that fails every write must see zero writes in plan mode.
    let fake = FakeDiscord::start(fixture(8, 10), 0).await;

    let labels = plan_labels(&scratch, &fake, &source, 2).await;
    assert!(
        labels.iter().any(|label| label.contains("Member")),
        "role op missing from plan: {labels:?}"
    );
    assert!(
        labels.iter().any(|label| label.contains("settings")),
        "settings op missing from plan: {labels:?}"
    );
    assert!(fake.writes().is_empty(), "plan mode wrote to Discord");
}
