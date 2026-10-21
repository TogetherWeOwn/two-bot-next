//! Acceptance: failed atomic JSON writes leave no partial `.tmp` behind.
//!
//! Contract (protected; the builder implements to satisfy these tests):
//! - Every `atomic_json` failure after the temp file is created removes its
//!   pid temp (scope guard). Callers surface the error; the directory holds no
//!   `.tmp` partials.
//! - A scope guard covers all post-create failures (write, permissions, fsync,
//!   rename). Rename is the reliably injectable representative via the CLI: an
//!   `--evidence` path that is a directory fails the rename. Three directory
//!   shapes plus a repeated-failure run pin the guard; write/permission/fsync
//!   failures are not reliably injectable without races or privileges.
//! - Success paths write byte-identical JSON and leave no `.tmp`.
//!
//! Offline only: loopback Discord, run-owned scratch, cleared child env.

use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Request, State};
use axum::http::{Method, StatusCode};
use axum::{Json, Router};
use serde_json::{Map, Value};
use tokio::process::Command;
use two_bot_core::backup::guild_config::{self, STAGING_BOT_APPLICATION_ID, TWO_STAGING_GUILD_ID};

const BOT_ROLE: &str = "100000000000000001";
const TARGET_ROLE: &str = "100000000000000002";

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::var_os("PAPERCLIP_RUN_SCRATCH_DIR")
            .or_else(|| std::env::var_os("PAPERCLIP_SCRATCH_DIR"))
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let dir = root.join(format!(
            "backup-tmp-cleanup-{}-{}-{}",
            tag,
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
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

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

fn fixture() -> Map<String, Value> {
    serde_json::json!({
        "version": 1,
        "applicationId": STAGING_BOT_APPLICATION_ID,
        "guildId": TWO_STAGING_GUILD_ID,
        "guild": {
            "id": TWO_STAGING_GUILD_ID,
            "owner_id": "100000000000000099",
            "name": "Loopback guild",
            // Explicit nulls: the planner compares channel-id fields by value,
            // and a missing key differs from null. Without these the identical
            // fixture plans 1 settings op, `--apply` attempts a Discord write
            // the loopback refuses (403), and the run never reaches `--evidence`.
            "system_channel_id": null,
            "rules_channel_id": null,
            "public_updates_channel_id": null,
            "afk_channel_id": null
        },
        "roles": [
            {"id": TWO_STAGING_GUILD_ID, "name": "@everyone", "position": 0, "permissions": "0", "managed": false},
            {"id": BOT_ROLE, "name": "Owen managed role", "position": 10, "permissions": "8", "managed": true},
            {"id": TARGET_ROLE, "name": "Member", "position": 5, "permissions": "0", "managed": false}
        ],
        "channels": [],
        "emojis": []
    })
    .as_object()
    .unwrap()
    .clone()
}

struct FakeState {
    current: Map<String, Value>,
    calls: Mutex<Vec<(Method, String)>>,
}

async fn fake_discord(
    State(state): State<Arc<FakeState>>,
    request: Request,
) -> (StatusCode, Json<Value>) {
    let expected = format!("Bot {}", fake_token());
    if request
        .headers()
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        != Some(expected.as_str())
    {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"message": "unexpected credential"})),
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
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"message": "writes refused by fixture"})),
        );
    }
    let guild_path = format!("/guilds/{TWO_STAGING_GUILD_ID}");
    let body = match path.as_str() {
        "/users/@me" => serde_json::json!({"id": STAGING_BOT_APPLICATION_ID}),
        "/users/@me/guilds" => serde_json::json!([{"id": TWO_STAGING_GUILD_ID}]),
        p if p == guild_path => state.current["guild"].clone(),
        p if p == format!("{guild_path}/roles") => state.current["roles"].clone(),
        p if p == format!("{guild_path}/channels") => state.current["channels"].clone(),
        p if p == format!("{guild_path}/emojis") => state.current["emojis"].clone(),
        p if p == format!("{guild_path}/members/{STAGING_BOT_APPLICATION_ID}") => {
            serde_json::json!({"roles": [BOT_ROLE]})
        }
        _ => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"message": "unexpected route"})),
            );
        }
    };
    (StatusCode::OK, Json(body))
}

struct FakeDiscord {
    base: String,
    task: tokio::task::JoinHandle<()>,
}

impl FakeDiscord {
    async fn start(current: Map<String, Value>) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(FakeState {
            current,
            calls: Mutex::new(Vec::new()),
        });
        let app = Router::new()
            .fallback(fake_discord)
            .with_state(Arc::clone(&state));
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { base, task }
    }
}

impl Drop for FakeDiscord {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// `guild-config-restore --apply` with identical source/live plans zero
/// operations, applies cleanly, then writes `--evidence`: the evidence write is
/// the injectable `atomic_json` seam.
fn restore_apply_cmd(
    scratch: &Scratch,
    fake: &FakeDiscord,
    snapshot: &Path,
    evidence: &Path,
) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_two-bot"));
    cmd.env_clear()
        .kill_on_drop(true)
        .current_dir(&scratch.0)
        .arg("guild-config-restore")
        .arg("--snapshot")
        .arg(snapshot)
        .arg("--confirm-staging-guild")
        .arg("--apply")
        .arg("--evidence")
        .arg(evidence)
        .env("CREDENTIALS_DIRECTORY", scratch.0.join("credentials"))
        .env("DISCORD_STAGING_GUILD_ID", TWO_STAGING_GUILD_ID)
        .env("GUILD_CONFIG_API_BASE", &fake.base)
        .env("GUILD_CONFIG_CDN_BASE", &fake.base)
        .env("TWO_GUILD_CONFIG_OFFLINE_TEST", "1");
    if let Some(profile) = std::env::var_os("LLVM_PROFILE_FILE") {
        cmd.env("LLVM_PROFILE_FILE", profile);
    }
    cmd
}

fn snapshot_cmd(scratch: &Scratch, fake: &FakeDiscord, out_dir: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_two-bot"));
    cmd.env_clear()
        .kill_on_drop(true)
        .current_dir(&scratch.0)
        .arg("guild-config-snapshot")
        .env("CREDENTIALS_DIRECTORY", scratch.0.join("credentials"))
        .env("DISCORD_STAGING_GUILD_ID", TWO_STAGING_GUILD_ID)
        .env("GUILD_CONFIG_API_BASE", &fake.base)
        .env("GUILD_CONFIG_CDN_BASE", &fake.base)
        .env("TWO_GUILD_CONFIG_OFFLINE_TEST", "1")
        .env("TWO_GUILD_CONFIG_BACKUP_DIR", out_dir)
        .env("TWO_GUILD_CONFIG_UPLOAD_CMD", "/bin/true");
    if let Some(profile) = std::env::var_os("LLVM_PROFILE_FILE") {
        cmd.env("LLVM_PROFILE_FILE", profile);
    }
    cmd
}

async fn output(cmd: &mut Command) -> (Output, String) {
    let output = tokio::time::timeout(Duration::from_secs(30), cmd.output())
        .await
        .expect("loopback CLI timed out")
        .expect("CLI failed to start");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!text.contains(&fake_token()), "CLI must not log a token");
    (output, text)
}

fn write_snapshot(path: &Path, source: &Map<String, Value>) {
    let sealed = guild_config::seal_snapshot(source.clone());
    std::fs::write(path, serde_json::to_vec(&sealed).unwrap()).unwrap();
}

/// Partial temps the writer must remove: pid-suffixed siblings ending in `.tmp`.
fn tmp_partials(dir: &Path) -> Vec<PathBuf> {
    let mut partials = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return partials;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file()
            && path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(".tmp"))
        {
            partials.push(path);
        }
    }
    partials
}

#[tokio::test]
async fn evidence_as_directory_leaves_no_tmp() {
    // RED today: the rename onto the directory fails and the pid temp leaks as
    // a sibling. Fixed: exit 1 with the error and zero `.tmp` partials.
    let scratch = Scratch::new("evidence-dir");
    scratch.write_token();
    let snapshot = scratch.0.join("source.json");
    write_snapshot(&snapshot, &fixture());
    let evidence = scratch.0.join("evidence");
    std::fs::create_dir(&evidence).unwrap();
    let fake = FakeDiscord::start(fixture()).await;
    let (out, text) = output(&mut restore_apply_cmd(
        &scratch, &fake, &snapshot, &evidence,
    ))
    .await;
    assert_eq!(out.status.code(), Some(1), "{text}");
    assert!(text.contains("cannot write evidence"), "{text}");
    assert!(
        tmp_partials(&scratch.0).is_empty(),
        "failed evidence write leaked {:?}: {text}",
        tmp_partials(&scratch.0)
    );
    assert!(evidence.is_dir(), "evidence directory must survive: {text}");
}

#[tokio::test]
async fn evidence_as_nonempty_directory_leaves_no_tmp() {
    // RED today: same rename failure with a non-empty target leaks the temp.
    // Fixed: the temp is removed and the directory contents are preserved.
    let scratch = Scratch::new("evidence-nonempty");
    scratch.write_token();
    let snapshot = scratch.0.join("source.json");
    write_snapshot(&snapshot, &fixture());
    let evidence = scratch.0.join("evidence.json");
    std::fs::create_dir(&evidence).unwrap();
    std::fs::write(evidence.join("keep.txt"), b"operator data\n").unwrap();
    let fake = FakeDiscord::start(fixture()).await;
    let (out, text) = output(&mut restore_apply_cmd(
        &scratch, &fake, &snapshot, &evidence,
    ))
    .await;
    assert_eq!(out.status.code(), Some(1), "{text}");
    assert!(text.contains("cannot write evidence"), "{text}");
    assert!(
        tmp_partials(&scratch.0).is_empty(),
        "failed evidence write leaked {:?}: {text}",
        tmp_partials(&scratch.0)
    );
    assert_eq!(
        std::fs::read(evidence.join("keep.txt")).unwrap(),
        b"operator data\n",
        "cleanup must not touch siblings: {text}"
    );
}

#[tokio::test]
async fn repeated_evidence_failures_leave_no_tmp() {
    // RED today: each failed run leaks its own pid temp, accumulating partials.
    // Fixed: two failures still leave zero `.tmp` files.
    let scratch = Scratch::new("evidence-repeat");
    scratch.write_token();
    let snapshot = scratch.0.join("source.json");
    write_snapshot(&snapshot, &fixture());
    let evidence = scratch.0.join("evidence-repeat");
    std::fs::create_dir(&evidence).unwrap();
    let fake = FakeDiscord::start(fixture()).await;
    for run in 0..2 {
        let (out, text) = output(&mut restore_apply_cmd(
            &scratch, &fake, &snapshot, &evidence,
        ))
        .await;
        assert_eq!(out.status.code(), Some(1), "run {run}: {text}");
    }
    assert!(
        tmp_partials(&scratch.0).is_empty(),
        "repeated failures leaked {:?}",
        tmp_partials(&scratch.0)
    );
}

#[tokio::test]
async fn evidence_success_writes_byte_identical_and_no_tmp() {
    // GREEN control: a regular `--evidence` file succeeds with the expected
    // receipt shape and no `.tmp` sibling.
    let scratch = Scratch::new("evidence-ok");
    scratch.write_token();
    let snapshot = scratch.0.join("source.json");
    write_snapshot(&snapshot, &fixture());
    let evidence = scratch.0.join("evidence.json");
    let fake = FakeDiscord::start(fixture()).await;
    let (out, text) = output(&mut restore_apply_cmd(
        &scratch, &fake, &snapshot, &evidence,
    ))
    .await;
    assert_eq!(out.status.code(), Some(0), "{text}");
    let receipt: Value =
        serde_json::from_slice(&std::fs::read(&evidence).unwrap()).expect("evidence JSON");
    assert_eq!(receipt.get("version").and_then(Value::as_u64), Some(1));
    assert_eq!(
        receipt.get("guildId").and_then(Value::as_str),
        Some(TWO_STAGING_GUILD_ID)
    );
    assert_eq!(
        receipt.get("hashesEqual").and_then(Value::as_bool),
        Some(true),
        "{text}"
    );
    assert!(
        tmp_partials(&scratch.0).is_empty(),
        "success left {:?}",
        tmp_partials(&scratch.0)
    );
}

#[tokio::test]
async fn snapshot_success_leaves_no_tmp() {
    // GREEN control: `guild-config-snapshot` writes two files and no `.tmp`.
    let scratch = Scratch::new("snapshot-ok");
    scratch.write_token();
    let fake = FakeDiscord::start(fixture()).await;
    let out_dir = scratch.0.join("captured");
    let (out, text) = output(&mut snapshot_cmd(&scratch, &fake, &out_dir)).await;
    assert!(out.status.success(), "{text}");
    let files: Vec<_> = std::fs::read_dir(&out_dir)
        .unwrap()
        .map(|f| f.unwrap().path())
        .collect();
    assert_eq!(files.len(), 2, "{text}");
    assert!(
        tmp_partials(&out_dir).is_empty(),
        "snapshot left {:?}: {text}",
        tmp_partials(&out_dir)
    );
}
