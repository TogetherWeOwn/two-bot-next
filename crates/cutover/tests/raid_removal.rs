use serde_json::{json, Value};
use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
    time::Instant,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use two_bot_core::raid_removal::RemovalMode;
use two_bot_cutover::raid_tools::{remove_accounts, FileAudit, RemovalRecord, RemovalRun};
use two_bot_discord::executor::{ActionExecutor, KICK_INTERVAL_MS};
const G: &str = "100000000000000010";
const BOT: &str = "100000000000000099";
const A: &str = "100000000000000001";
const B: &str = "100000000000000002";
type Step = (String, u16, Value);
fn step(path: String, status: u16, body: Value) -> Step {
    (path, status, body)
}
fn safety(id: &str, roles: Value) -> Vec<Step> {
    vec![
        step(
            format!("GET /api/v10/guilds/{G}/members/{id}"),
            200,
            json!({"user":{"id":id,"bot":false},"roles":roles}),
        ),
        step(
            format!("GET /api/v10/guilds/{G}"),
            200,
            json!({"owner_id":"100000000000000088"}),
        ),
        step("GET /api/v10/users/@me".into(), 200, json!({"id":BOT})),
        step(
            format!("GET /api/v10/guilds/{G}/roles"),
            200,
            json!([
                {"id":G,"position":0,"permissions":"0"},
                {"id":"100000000000000050","position":5,"permissions":"2"},
                {"id":"100000000000000060","position":1,"permissions":"8"}
            ]),
        ),
        step(
            format!("GET /api/v10/guilds/{G}/members/{BOT}"),
            200,
            json!({"roles":["100000000000000050"]}),
        ),
    ]
}
async fn mock(
    steps: Vec<Step>,
) -> (
    ActionExecutor,
    Arc<Mutex<Vec<(String, Instant)>>>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let recorded = Arc::new(Mutex::new(vec![]));
    let requests = recorded.clone();
    let handle = tokio::spawn(async move {
        for (expected, status, value) in steps {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = vec![];
            let mut chunk = [0; 4096];
            loop {
                let n = stream.read(&mut chunk).await.unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&chunk[..n]);
                if bytes.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let text = String::from_utf8(bytes).unwrap();
            let first = text
                .lines()
                .next()
                .unwrap()
                .strip_suffix(" HTTP/1.1")
                .unwrap()
                .to_owned();
            assert_eq!(first, expected);
            if first.starts_with("DELETE") {
                assert!(text.to_ascii_lowercase().contains("x-audit-log-reason:"));
            }
            requests.lock().unwrap().push((first, Instant::now()));
            let body = if status == 204 {
                String::new()
            } else {
                value.to_string()
            };
            let response = format!("HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            stream.write_all(response.as_bytes()).await.unwrap();
        }
    });
    (
        ActionExecutor::with_proxy("offline-fixture-token".into(), Some(base)).unwrap(),
        recorded,
        handle,
    )
}

#[tokio::test]
async fn execute_revalidates_protection_paces_retries_and_audits_each_reached_target() {
    let mut steps = safety(A, json!([]));
    steps.push(step(
        format!("DELETE /api/v10/guilds/{G}/members/{A}"),
        500,
        json!({}),
    ));
    steps.push(step(
        format!("DELETE /api/v10/guilds/{G}/members/{A}"),
        204,
        Value::Null,
    ));
    steps.extend(safety(B, json!(["100000000000000060"])));
    steps.push(step(
        format!("GET /api/v10/guilds/{G}/members/100000000000000003"),
        404,
        json!({}),
    ));
    let (ex, requests, task) = mock(steps).await;
    let ids = vec![A.into(), B.into(), "100000000000000003".into()];
    let empty = HashSet::new();
    let mut records = vec![];
    let summary = remove_accounts(
        RemovalRun {
            guild: G,
            ids: &ids,
            mode: RemovalMode::Execute,
            reason: "reviewed cohort",
            run_id: "test",
            done: &empty,
            protected: &empty,
        },
        Some(&ex),
        |r| {
            records.push(r.clone());
            Ok(())
        },
    )
    .await
    .unwrap();
    task.await.unwrap();
    assert_eq!(summary.reached, 3);
    assert_eq!(summary.failed, 0);
    assert_eq!(
        records
            .iter()
            .map(|r| r.outcome.as_str())
            .collect::<Vec<_>>(),
        vec!["kicked", "protected", "already_gone"]
    );
    assert_eq!(records[0].attempts, 2);
    let requests = requests.lock().unwrap();
    let kicks: Vec<_> = requests
        .iter()
        .filter(|(p, _)| p.starts_with("DELETE"))
        .collect();
    assert_eq!(kicks.len(), 2);
    assert!(kicks[1].1.duration_since(kicks[0].1).as_millis() >= u128::from(KICK_INTERVAL_MS));
    assert_eq!(KICK_INTERVAL_MS, 350);
}

#[tokio::test]
async fn forbidden_read_is_not_missing_and_aborts_without_credential_retry_or_delete() {
    let (ex, requests, task) = mock(vec![step(
        format!("GET /api/v10/guilds/{G}/members/{A}"),
        403,
        json!({}),
    )])
    .await;
    let ids = vec![A.into(), B.into()];
    let empty = HashSet::new();
    let mut records = vec![];
    let summary = remove_accounts(
        RemovalRun {
            guild: G,
            ids: &ids,
            mode: RemovalMode::Execute,
            reason: "reviewed",
            run_id: "test",
            done: &empty,
            protected: &empty,
        },
        Some(&ex),
        |r| {
            records.push(r.clone());
            Ok(())
        },
    )
    .await
    .unwrap();
    task.await.unwrap();
    assert!(summary.aborted);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].outcome, "failed");
    assert_eq!(requests.lock().unwrap().len(), 1);
}

#[test]
fn file_audit_is_durable_locked_and_terminal_ever_scoped_by_guild() {
    let root = std::env::var_os("PAPERCLIP_SCRATCH_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let path = root.join(format!(
        "raid-audit-test-{}-{}.jsonl",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let mut audit = FileAudit::open(&path, G).unwrap();
    assert!(FileAudit::open(&path, G).is_err());
    let mut r = RemovalRecord {
        v: 1,
        ts: "2026-09-01T00:00:00Z".into(),
        run_id: "test".into(),
        guild_id: G.into(),
        member_id: A.into(),
        mode: RemovalMode::Execute,
        action: "kick".into(),
        outcome: "kicked".into(),
        status: Some(204),
        attempts: 1,
    };
    audit.append(&r).unwrap();
    r.mode = RemovalMode::DryRun;
    r.outcome = "would_kick".into();
    audit.append(&r).unwrap();
    drop(audit);
    let audit = FileAudit::open(&path, G).unwrap();
    assert!(audit.done.contains(A));
    drop(audit);
    let audit = FileAudit::open(&path, "100000000000000011").unwrap();
    assert!(audit.done.is_empty());
    drop(audit);
    assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 2);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn both_clis_refuse_live_before_file_or_database_access_and_help_is_offline() {
    for bin in [
        env!("CARGO_BIN_EXE_raid-list"),
        env!("CARGO_BIN_EXE_raid-remove"),
    ] {
        let out = std::process::Command::new(bin)
            .args(["--guild", two_bot_cutover::LIVE_GUILD_ID])
            .env_remove("TWO_DATABASE_URL")
            .env_remove("DISCORD_TOKEN")
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2));
        assert!(String::from_utf8(out.stderr)
            .unwrap()
            .contains("Refusing live guild"));
        assert!(std::process::Command::new(bin)
            .arg("--help")
            .output()
            .unwrap()
            .status
            .success());
    }
}
