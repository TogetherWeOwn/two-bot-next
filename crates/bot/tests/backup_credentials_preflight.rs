//! Real CLI tests against loopback only: credential custody and live authority.

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
use two_bot_core::backup::guild_config::{self, STAGING_BOT_APPLICATION_ID, TWO_STAGING_GUILD_ID};

const BOT_ROLE: &str = "100000000000000001";
const TARGET_ROLE: &str = "100000000000000002";
const CHANNEL: &str = "100000000000000003";
const WRITE_SENTINEL: &str = "loopback write reached; deliberately refused";

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

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::var_os("PAPERCLIP_RUN_SCRATCH_DIR")
            .or_else(|| std::env::var_os("PAPERCLIP_SCRATCH_DIR"))
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let dir = root.join(format!(
            "backup-credential-cli-{}-{}",
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

struct FakeState {
    current: Map<String, Value>,
    calls: Mutex<Vec<(Method, String)>>,
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
        // Reaching this sentinel proves preflight passed, without simulating a
        // full restore or obscuring the assertion with post-restore hash drift.
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"message": WRITE_SENTINEL})),
        );
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
        // Source: https://docs.rs/axum/0.8/axum/fn.serve.html
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { base, state, task }
    }

    fn writes(&self) -> usize {
        self.state
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(m, _)| *m != Method::GET)
            .count()
    }

    fn assert_member_preflight(&self) {
        let path = format!("/guilds/{TWO_STAGING_GUILD_ID}/members/{STAGING_BOT_APPLICATION_ID}");
        assert!(self
            .state
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|(m, p)| *m == Method::GET && *p == path));
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
        // Snapshot success requires two uploads. This local no-op performs no
        // network call and inherits only this explicitly scrubbed environment.
        .env("TWO_GUILD_CONFIG_UPLOAD_CMD", "/bin/true");
    cmd
}

async fn output(mut cmd: Command) -> (Output, String) {
    let output = tokio::time::timeout(Duration::from_secs(15), cmd.output())
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

fn restore_command(scratch: &Scratch, fake: &FakeDiscord, source: &Path) -> Command {
    let mut cmd = cli(scratch, fake, "guild-config-restore");
    cmd.arg("--snapshot")
        .arg(source)
        .arg("--confirm-staging-guild")
        .arg("--apply");
    cmd
}

#[tokio::test]
async fn unflagged_loopback_requires_admission_before_any_request() {
    let scratch = Scratch::new();
    scratch.write_token();
    let source = scratch.snapshot(fixture(8, 10));
    let fake = FakeDiscord::start(fixture(8, 10)).await;
    for mut cmd in [
        cli(&scratch, &fake, "guild-config-snapshot"),
        restore_command(&scratch, &fake, &source),
    ] {
        cmd.env_remove("TWO_GUILD_CONFIG_OFFLINE_TEST");
        let (out, text) = output(cmd).await;
        assert_eq!(out.status.code(), Some(2), "{text}");
        assert!(
            text.contains("TWO_DATABASE_URL admission authority required"),
            "{text}"
        );
    }
    assert!(fake.state.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn offline_flag_requires_both_explicit_loopback_endpoints() {
    let scratch = Scratch::new();
    scratch.write_token();
    let fake = FakeDiscord::start(fixture(8, 10)).await;
    for name in ["GUILD_CONFIG_API_BASE", "GUILD_CONFIG_CDN_BASE"] {
        let mut cmd = cli(&scratch, &fake, "guild-config-snapshot");
        cmd.env_remove(name);
        let (out, text) = output(cmd).await;
        assert_eq!(out.status.code(), Some(2), "{text}");
        assert!(text.contains("offline fixture"), "{text}");

        let mut cmd = cli(&scratch, &fake, "guild-config-snapshot");
        cmd.env(name, "https://example.invalid");
        let (out, text) = output(cmd).await;
        assert_eq!(out.status.code(), Some(2), "{text}");
        assert!(text.contains("only accepts loopback"), "{text}");
    }
    assert!(fake.state.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn supplied_empty_authority_is_not_bypassed_by_offline_flag() {
    let scratch = Scratch::new();
    scratch.write_token();
    let fake = FakeDiscord::start(fixture(8, 10)).await;
    let mut cmd = cli(&scratch, &fake, "guild-config-snapshot");
    cmd.env("TWO_DATABASE_URL", "");
    let (out, text) = output(cmd).await;
    assert_eq!(out.status.code(), Some(2), "{text}");
    assert!(
        text.contains("TWO_DATABASE_URL admission authority required"),
        "{text}"
    );
    assert!(fake.state.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn credential_file_only_snapshot_reaches_loopback_and_seals_capture() {
    let scratch = Scratch::new();
    scratch.write_token();
    let fake = FakeDiscord::start(fixture(8, 10)).await;
    let (out, text) = output(cli(&scratch, &fake, "guild-config-snapshot")).await;
    assert!(out.status.success(), "{text}");
    assert!(
        text.contains("snapshot and drift report uploaded"),
        "{text}"
    );
    assert_eq!(fake.state.calls.lock().unwrap().len(), 6);
    assert_eq!(fake.writes(), 0);
    let files: Vec<_> = std::fs::read_dir(scratch.0.join("captured"))
        .unwrap()
        .map(|f| f.unwrap().path())
        .collect();
    assert_eq!(files.len(), 2);
    let snapshot = files
        .iter()
        .find(|p| !p.to_string_lossy().ends_with(".drift.json"))
        .unwrap();
    let contents = std::fs::read_to_string(snapshot).unwrap();
    assert!(!contents.contains(&fake_token()));
    let captured: Map<String, Value> = serde_json::from_str(&contents).unwrap();
    assert_eq!(
        guild_config::verify_snapshot_integrity(&captured).unwrap(),
        guild_config::SealState::Sealed
    );
}

#[tokio::test]
async fn stale_administrator_removed_live_refuses_before_any_discord_write() {
    let scratch = Scratch::new();
    scratch.write_token();
    let mut source = fixture(8, 10);
    source["guild"]["name"] = json!("Restored name");
    let source = scratch.snapshot(source);
    let fake = FakeDiscord::start(fixture(0, 10)).await;
    let (out, text) = output(restore_command(&scratch, &fake, &source)).await;
    assert_eq!(out.status.code(), Some(1), "{text}");
    assert!(
        text.contains("Restore permission preflight failed: missing Manage Guild"),
        "{text}"
    );
    fake.assert_member_preflight();
    assert_eq!(fake.writes(), 0);
}

#[tokio::test]
async fn administrator_added_live_passes_preflight_despite_stale_snapshot() {
    let scratch = Scratch::new();
    scratch.write_token();
    let mut source = fixture(0, 10);
    source["guild"]["name"] = json!("Restored name");
    let source = scratch.snapshot(source);
    let fake = FakeDiscord::start(fixture(8, 10)).await;
    let (out, text) = output(restore_command(&scratch, &fake, &source)).await;
    assert_eq!(out.status.code(), Some(1), "{text}");
    assert!(
        text.contains("apply failed")
            && text.contains("HTTP 403")
            && !text.contains(WRITE_SENTINEL),
        "{text}"
    );
    assert!(!text.contains("preflight failed"), "{text}");
    fake.assert_member_preflight();
    assert_eq!(fake.writes(), 1);
}

async fn hierarchy_case(source_position: i64, live_position: i64, allowed: bool) {
    let scratch = Scratch::new();
    scratch.write_token();
    let mut source = fixture(8, source_position);
    let channels = json!([{"id": CHANNEL, "name": "general", "type": 0, "position": 0,
        "parent_id": null, "permission_overwrites": [{"id": TARGET_ROLE, "type": 0, "allow": "1024", "deny": "0"}]}]);
    source.insert("channels".to_owned(), channels.clone());
    let source = scratch.snapshot(source);
    let mut live = fixture(8, live_position);
    live.insert("channels".to_owned(), channels);
    live["channels"][0]["permission_overwrites"] = json!([]);
    let fake = FakeDiscord::start(live).await;
    let (out, text) = output(restore_command(&scratch, &fake, &source)).await;
    assert_eq!(out.status.code(), Some(1), "{text}");
    fake.assert_member_preflight();
    if allowed {
        assert!(
            text.contains("apply failed")
                && text.contains("HTTP 403")
                && !text.contains(WRITE_SENTINEL),
            "{text}"
        );
        assert!(!text.contains("preflight failed"), "{text}");
        assert_eq!(fake.writes(), 1);
    } else {
        assert!(
            text.contains("Restore hierarchy preflight failed"),
            "{text}"
        );
        assert_eq!(fake.writes(), 0);
    }
}

#[tokio::test]
async fn bot_demoted_live_refuses_overwrite_despite_stale_higher_role() {
    hierarchy_case(10, 3, false).await;
}

#[tokio::test]
async fn bot_promoted_live_passes_overwrite_preflight_despite_stale_lower_role() {
    hierarchy_case(3, 10, true).await;
}

#[tokio::test]
async fn bad_credential_refuses_both_commands_even_with_valid_environment_fallback() {
    let scratch = Scratch::new();
    let source = scratch.snapshot(fixture(8, 10));
    let fake = FakeDiscord::start(fixture(8, 10)).await;
    for contents in [
        &b" \n"[..],
        &b"invalid-secret-marker"[..],
        &b"\xffsecret-marker"[..],
    ] {
        std::fs::write(scratch.credential(), contents).unwrap();
        for mut cmd in [
            cli(&scratch, &fake, "guild-config-snapshot"),
            restore_command(&scratch, &fake, &source),
        ] {
            cmd.env("DISCORD_STAGING_BOT_TOKEN", fake_token());
            let (out, text) = output(cmd).await;
            assert!(!out.status.success(), "{text}");
            assert!(text.contains("refusing environment fallback"), "{text}");
            assert!(!text.contains("secret-marker"), "{text}");
        }
    }
    std::fs::remove_file(scratch.credential()).unwrap();
    std::fs::create_dir(scratch.credential()).unwrap();
    for mut cmd in [
        cli(&scratch, &fake, "guild-config-snapshot"),
        restore_command(&scratch, &fake, &source),
    ] {
        cmd.env("DISCORD_STAGING_BOT_TOKEN", fake_token());
        let (out, text) = output(cmd).await;
        assert!(!out.status.success(), "{text}");
        assert!(text.contains("cannot read discord_staging_token"), "{text}");
    }
    assert!(
        fake.state.calls.lock().unwrap().is_empty(),
        "refusal must precede every Discord call"
    );
}

#[tokio::test]
async fn absent_credential_allows_explicit_snapshot_environment_fallback() {
    let scratch = Scratch::new();
    let fake = FakeDiscord::start(fixture(8, 10)).await;
    for configured_directory in [true, false] {
        let mut cmd = cli(&scratch, &fake, "guild-config-snapshot");
        cmd.env("DISCORD_STAGING_BOT_TOKEN", fake_token());
        if !configured_directory {
            cmd.env_remove("CREDENTIALS_DIRECTORY");
        }
        let (out, text) = output(cmd).await;
        assert!(out.status.success(), "{text}");
    }
    assert_eq!(fake.state.calls.lock().unwrap().len(), 12);
    assert_eq!(fake.writes(), 0);
}
