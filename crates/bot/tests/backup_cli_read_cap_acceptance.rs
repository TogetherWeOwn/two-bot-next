//! Acceptance: operator-supplied backup files refuse past their caps before buffering.
//!
//! Contract (protected; the builder implements to satisfy these tests):
//! - `backup-upload` refuses dumps past `dump_file::MAX_DUMP_BYTES` (1 GiB) with
//!   exit 1, naming the byte cap, before any S3 PUT.
//! - `guild-config-restore --snapshot` refuses snapshots past 8 MiB (8388608 bytes,
//!   mirroring `http::MAX_RESPONSE_BYTES`) with exit 2, naming the byte cap,
//!   before any Discord read. Guild snapshots are KB-scale; 8 MiB refuses abuse
//!   without breaking lawful restores.
//! - Small inputs succeed byte-identical; empty dumps still refuse as today.
//! - Boundary is strict `>`: exactly-at-cap inputs still pass.
//!
//! Offline only: loopback fakes, run-owned scratch, cleared child env. No DB,
//! S3, or Discord beyond loopback.

use std::path::PathBuf;
use std::process::Output;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Request, State};
use axum::http::{Method, StatusCode};
use axum::{Json, Router};
use serde_json::{Map, Value};
use tokio::process::Command;
use two_bot_core::backup::dump_file::MAX_DUMP_BYTES;
use two_bot_core::backup::guild_config::{self, STAGING_BOT_APPLICATION_ID, TWO_STAGING_GUILD_ID};

/// Snapshot read cap the builder implements: 8 MiB, mirroring
/// `http::MAX_RESPONSE_BYTES`. Guild snapshots are KB-scale.
const EXPECTED_SNAPSHOT_CAP_BYTES: u64 = 8 * 1024 * 1024;

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
            "backup-read-cap-{}-{}-{}",
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

/// Unusable token derived from the public application id, never a real secret.
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
        "guild": {"id": TWO_STAGING_GUILD_ID, "owner_id": "100000000000000099", "name": "Loopback guild"},
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
    state: Arc<FakeState>,
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
        Self { base, state, task }
    }

    fn calls(&self) -> usize {
        self.state.calls.lock().unwrap().len()
    }
}

impl Drop for FakeDiscord {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Raw S3 PUT counter: reads only the request head, counts the attempt, then
/// closes without reading the body. An oversized RED run that buffers 1 GiB
/// still fails fast here instead of shipping it.
struct S3PutCounter {
    port: u16,
    hits: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl S3PutCounter {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let hits = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&hits);
        let task = tokio::spawn(async move {
            use tokio::io::AsyncReadExt as _;
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let seen = Arc::clone(&seen);
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let mut head = Vec::new();
                    loop {
                        match socket.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => {
                                head.extend_from_slice(&buf[..n]);
                                if head.windows(4).any(|w| w == b"\r\n\r\n") || head.len() > 65536 {
                                    break;
                                }
                            }
                        }
                    }
                    if head.starts_with(b"PUT ") {
                        seen.fetch_add(1, Ordering::Relaxed);
                    }
                    // Close without reading the body: the client sees a reset.
                });
            }
        });
        Self { port, hits, task }
    }

    fn hits(&self) -> usize {
        self.hits.load(Ordering::Relaxed)
    }
}

impl Drop for S3PutCounter {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Raw S3 capture fake for the small-file control: reads the full PUT body and
/// answers 200 so the upload succeeds byte-identical.
struct S3Capture {
    port: u16,
    bodies: Arc<Mutex<Vec<Vec<u8>>>>,
    task: tokio::task::JoinHandle<()>,
}

impl S3Capture {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let bodies: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
        let held = Arc::clone(&bodies);
        let task = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let held = Arc::clone(&held);
                tokio::spawn(async move {
                    let mut head = Vec::new();
                    let mut byte = [0u8; 1];
                    while !head.windows(4).any(|w| w == b"\r\n\r\n") && head.len() < 65536 {
                        match socket.read(&mut byte).await {
                            Ok(1) => head.push(byte[0]),
                            _ => return,
                        }
                    }
                    let head_text = String::from_utf8_lossy(&head);
                    let len = head_text
                        .lines()
                        .find_map(|line| {
                            line.strip_prefix("content-length:")
                                .or_else(|| line.strip_prefix("Content-Length:"))
                                .and_then(|v| v.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    let mut body = vec![0u8; len.min(16 * 1024 * 1024)];
                    if len > body.len() {
                        return;
                    }
                    let mut read = 0;
                    while read < body.len() {
                        match socket.read(&mut body[read..]).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => read += n,
                        }
                    }
                    held.lock().unwrap().push(body);
                    let _ = socket
                        .write_all(
                            b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok",
                        )
                        .await;
                });
            }
        });
        Self { port, bodies, task }
    }
}

impl Drop for S3Capture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn upload_cmd(scratch: &Scratch, endpoint: &str, dump: &std::path::Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_two-bot"));
    cmd.env_clear()
        .kill_on_drop(true)
        .current_dir(&scratch.0)
        .arg("backup-upload")
        .arg(dump)
        .env("TWO_BACKUP_S3_ENDPOINT", endpoint)
        .env("TWO_BACKUP_S3_BUCKET", "test-bucket")
        .env("TWO_BACKUP_S3_ACCESS_KEY_ID", "test-key")
        .env("TWO_BACKUP_S3_SECRET_ACCESS_KEY", "test-secret")
        .env("TWO_BACKUP_S3_TIMEOUT_MS", "10000");
    if let Some(profile) = std::env::var_os("LLVM_PROFILE_FILE") {
        cmd.env("LLVM_PROFILE_FILE", profile);
    }
    cmd
}

fn restore_cmd(scratch: &Scratch, fake: &FakeDiscord, snapshot: &std::path::Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_two-bot"));
    cmd.env_clear()
        .kill_on_drop(true)
        .current_dir(&scratch.0)
        .arg("guild-config-restore")
        .arg("--snapshot")
        .arg(snapshot)
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

async fn output(cmd: &mut Command, secs: u64) -> (Output, String) {
    let output = tokio::time::timeout(Duration::from_secs(secs), cmd.output())
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

fn write_snapshot(path: &std::path::Path, source: &Map<String, Value>) {
    let sealed = guild_config::seal_snapshot(source.clone());
    std::fs::write(path, serde_json::to_vec(&sealed).unwrap()).unwrap();
}

/// Pad a sealed snapshot with a filler string so the file is exactly `size`
/// bytes. Returns the bytes written.
fn write_padded_snapshot(path: &std::path::Path, size: usize) -> Vec<u8> {
    let mut source = fixture();
    // Placeholder replaced below once the base length is known.
    source.insert("padding".to_owned(), Value::String(String::new()));
    let sealed = guild_config::seal_snapshot(source);
    let mut bytes = serde_json::to_vec(&sealed).unwrap();
    // `"padding":""` contributes 13 bytes; grow the filler to hit `size`.
    let base_without_filler = bytes.len();
    assert!(
        size > base_without_filler,
        "target {size} must exceed base {base_without_filler}"
    );
    let filler_len = size - base_without_filler;
    let mut source = fixture();
    source.insert("padding".to_owned(), Value::String("x".repeat(filler_len)));
    let sealed = guild_config::seal_snapshot(source);
    bytes = serde_json::to_vec(&sealed).unwrap();
    assert_eq!(bytes.len(), size, "padded snapshot must hit {size} exactly");
    std::fs::write(path, &bytes).unwrap();
    bytes
}

#[tokio::test]
async fn oversized_dump_refuses_before_s3_put() {
    // RED today: `backup-upload` reads the whole file before signing, so the
    // loopback S3 sees a PUT and stderr carries a transport failure, not a cap
    // refusal. Fixed: exit 1 naming the 1 GiB cap with zero PUTs.
    assert_eq!(MAX_DUMP_BYTES, 1024 * 1024 * 1024u64);
    let scratch = Scratch::new("dump-oversize");
    let dump = scratch.0.join("oversize.ndjson.gz");
    std::fs::File::create(&dump)
        .unwrap()
        .set_len(MAX_DUMP_BYTES + 1)
        .unwrap();
    assert_eq!(std::fs::metadata(&dump).unwrap().len(), MAX_DUMP_BYTES + 1);
    let s3 = S3PutCounter::start().await;
    let endpoint = format!("http://127.0.0.1:{}", s3.port);
    let (out, text) = output(&mut upload_cmd(&scratch, &endpoint, &dump), 60).await;
    assert_eq!(out.status.code(), Some(1), "{text}");
    assert!(
        text.to_lowercase().contains("exceed"),
        "refusal must name the over-cap reason: {text}"
    );
    assert!(
        text.contains(&MAX_DUMP_BYTES.to_string()),
        "refusal must name the byte cap {}: {text}",
        MAX_DUMP_BYTES
    );
    // Settle the counter: the child exited, so any in-flight PUT has landed.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        s3.hits(),
        0,
        "oversized dump must refuse before any PUT: {text}"
    );
}

#[tokio::test]
async fn oversized_snapshot_refuses_before_discord_read() {
    // RED today: `guild-config-restore` reads the whole snapshot before the
    // cap, plans against loopback, and exits 0. Fixed: exit 2 naming the 8 MiB
    // cap with zero Discord calls.
    let scratch = Scratch::new("snapshot-oversize");
    scratch.write_token();
    let snapshot = scratch.0.join("oversize.json");
    write_padded_snapshot(&snapshot, EXPECTED_SNAPSHOT_CAP_BYTES as usize + 1);
    let fake = FakeDiscord::start(fixture()).await;
    let (out, text) = output(&mut restore_cmd(&scratch, &fake, &snapshot), 30).await;
    assert_eq!(out.status.code(), Some(2), "{text}");
    assert!(
        text.to_lowercase().contains("exceed"),
        "refusal must name the over-cap reason: {text}"
    );
    assert!(
        text.contains(&EXPECTED_SNAPSHOT_CAP_BYTES.to_string()),
        "refusal must name the byte cap {}: {text}",
        EXPECTED_SNAPSHOT_CAP_BYTES
    );
    assert_eq!(
        fake.calls(),
        0,
        "oversized snapshot must refuse before any Discord read: {text}"
    );
}

#[tokio::test]
async fn fifo_snapshot_refuses_without_blocking() {
    // A writer-less FIFO opened for reading blocks forever, so the restore
    // path must refuse it via metadata before `File::open`. Fixed: exit 2
    // naming the non-regular cause with zero Discord calls. The bounded
    // `output` wait turns a regression into a test failure (killing the
    // blocked child via `kill_on_drop`) instead of a hung job.
    let scratch = Scratch::new("snapshot-fifo");
    scratch.write_token();
    let fifo = scratch.0.join("snapshot.fifo");
    let status = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("mkfifo must exist on test hosts");
    assert!(status.success(), "mkfifo must create the FIFO");
    assert!(!std::fs::metadata(&fifo).unwrap().is_file());
    let fake = FakeDiscord::start(fixture()).await;
    let (out, text) = output(&mut restore_cmd(&scratch, &fake, &fifo), 30).await;
    assert_eq!(out.status.code(), Some(2), "{text}");
    assert!(
        text.contains("not a regular file"),
        "FIFO refusal must name the non-regular cause: {text}"
    );
    assert_eq!(
        fake.calls(),
        0,
        "FIFO snapshot must refuse before any Discord read: {text}"
    );
}

#[tokio::test]
async fn small_dump_uploads_byte_identical() {
    // GREEN control: a small dump still uploads, and the PUT body is the file.
    let scratch = Scratch::new("dump-small");
    let dump = scratch.0.join("small.ndjson.gz");
    let body = b"small dump fixture bytes\n".to_vec();
    std::fs::write(&dump, &body).unwrap();
    let s3 = S3Capture::start().await;
    let endpoint = format!("http://127.0.0.1:{}", s3.port);
    let (out, text) = output(&mut upload_cmd(&scratch, &endpoint, &dump), 30).await;
    assert!(out.status.success(), "{text}");
    assert!(text.contains("stored"), "{text}");
    let bodies = s3.bodies.lock().unwrap();
    assert_eq!(bodies.len(), 1, "{text}");
    assert_eq!(bodies[0], body, "PUT body must be byte-identical");
}

#[tokio::test]
async fn small_snapshot_plans_successfully() {
    // GREEN control: a small snapshot still plans against loopback.
    let scratch = Scratch::new("snapshot-small");
    scratch.write_token();
    let snapshot = scratch.0.join("small.json");
    write_snapshot(&snapshot, &fixture());
    let fake = FakeDiscord::start(fixture()).await;
    let (out, text) = output(&mut restore_cmd(&scratch, &fake, &snapshot), 30).await;
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert!(text.contains("planned"), "{text}");
    assert!(
        fake.calls() > 0,
        "small snapshot must still read Discord: {text}"
    );
}

#[tokio::test]
async fn empty_dump_still_refused() {
    // GREEN control: the existing empty-file refusal stays.
    let scratch = Scratch::new("dump-empty");
    let dump = scratch.0.join("empty.ndjson.gz");
    std::fs::write(&dump, b"").unwrap();
    let s3 = S3PutCounter::start().await;
    let endpoint = format!("http://127.0.0.1:{}", s3.port);
    let (out, text) = output(&mut upload_cmd(&scratch, &endpoint, &dump), 30).await;
    assert_eq!(out.status.code(), Some(1), "{text}");
    assert!(text.contains("empty"), "{text}");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(s3.hits(), 0, "{text}");
}

#[tokio::test]
async fn snapshot_at_cap_boundary_still_plans() {
    // GREEN control pinning strict `>`: exactly 8 MiB still plans.
    let scratch = Scratch::new("snapshot-boundary");
    scratch.write_token();
    let snapshot = scratch.0.join("boundary.json");
    write_padded_snapshot(&snapshot, EXPECTED_SNAPSHOT_CAP_BYTES as usize);
    let fake = FakeDiscord::start(fixture()).await;
    let (out, text) = output(&mut restore_cmd(&scratch, &fake, &snapshot), 30).await;
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert!(text.contains("planned"), "{text}");
    assert!(
        fake.calls() > 0,
        "boundary snapshot must still read Discord: {text}"
    );
}
