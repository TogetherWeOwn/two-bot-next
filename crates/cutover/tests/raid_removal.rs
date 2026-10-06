use serde_json::{json, Value};
use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use two_bot_core::raid_removal::RemovalMode;
use two_bot_cutover::raid_tools::{
    remove_accounts, FileAudit, RemovalOutcome, RemovalRecord, RemovalRun,
};
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
        step(
            format!("GET /api/v10/guilds/{G}/members/{id}"),
            200,
            json!({"user":{"id":id,"bot":false},"roles":roles}),
        ),
    ]
}
async fn read_request(stream: &mut TcpStream) -> String {
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
        .unwrap();
    if first.starts_with("DELETE") {
        assert!(text.to_ascii_lowercase().contains("x-audit-log-reason:"));
    }
    first.to_owned()
}
async fn respond(stream: &mut TcpStream, status: u16, value: &Value) {
    let body = if status == 204 {
        String::new()
    } else {
        value.to_string()
    };
    let response = format!("HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
    stream.write_all(response.as_bytes()).await.unwrap();
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
            let first = read_request(&mut stream).await;
            assert_eq!(first, expected);
            requests.lock().unwrap().push((first, Instant::now()));
            respond(&mut stream, status, &value).await;
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
    steps.extend(safety(A, json!([])));
    steps.push(step(
        format!("DELETE /api/v10/guilds/{G}/members/{A}"),
        204,
        Value::Null,
    ));
    steps.extend(safety(B, json!(["100000000000000060"])));
    let mut missing = safety("100000000000000003", json!([]));
    missing.last_mut().unwrap().1 = 404;
    missing.last_mut().unwrap().2 = json!({});
    steps.extend(missing);
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

async fn run_single(
    ex: &ActionExecutor,
    protected: &HashSet<String>,
) -> (
    two_bot_cutover::raid_tools::RemovalSummary,
    Vec<RemovalRecord>,
) {
    let ids = vec![A.into()];
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
            protected,
        },
        Some(ex),
        |r| {
            records.push(r.clone());
            Ok(())
        },
    )
    .await
    .unwrap();
    (summary, records)
}

#[tokio::test]
async fn webhook_only_staff_is_protected_without_a_delete() {
    let mut steps = safety(A, json!(["100000000000000060"]));
    steps[2].2[2]["permissions"] = json!((1_u64 << 27).to_string());
    let (ex, requests, task) = mock(steps).await;
    let (summary, records) = run_single(&ex, &HashSet::new()).await;
    task.await.unwrap();
    assert_eq!(summary.failed, 0);
    assert_eq!(records[0].outcome, RemovalOutcome::Protected);
    assert_eq!(records[0].attempts, 0);
    assert!(requests
        .lock()
        .unwrap()
        .iter()
        .all(|(p, _)| !p.starts_with("DELETE")));
}

#[tokio::test]
async fn retries_reread_configured_protection_and_never_delete_a_newly_protected_target() {
    for status in [500, 429] {
        let mut steps = safety(A, json!([]));
        steps[2].2[2]["permissions"] = json!("0");
        steps.push(step(
            format!("DELETE /api/v10/guilds/{G}/members/{A}"),
            status,
            json!({"retry_after":0.001}),
        ));
        let mut fresh = safety(A, json!(["100000000000000060"]));
        fresh[2].2[2]["permissions"] = json!("0");
        steps.extend(fresh);
        let (ex, requests, task) = mock(steps).await;
        let protected = HashSet::from(["100000000000000060".into()]);
        let (summary, records) = run_single(&ex, &protected).await;
        task.await.unwrap();
        assert!(!summary.aborted);
        assert_eq!(summary.failed, 0);
        assert_eq!(records[0].outcome, RemovalOutcome::Protected);
        assert_eq!(records[0].attempts, 1);
        assert_eq!(
            requests
                .lock()
                .unwrap()
                .iter()
                .filter(|(p, _)| p.starts_with("DELETE"))
                .count(),
            1
        );
    }
}

#[tokio::test]
async fn protection_assigned_during_metadata_read_is_seen_by_final_target_read() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let mut assigned = false;
        let mut requests = vec![];
        let mut steps = safety(A, json!([]));
        steps[2].2[2]["permissions"] = json!("0");
        for (expected, status, mut body) in steps {
            let (mut stream, _) = listener.accept().await.unwrap();
            let first = read_request(&mut stream).await;
            assert_eq!(first, expected);
            if first == format!("GET /api/v10/guilds/{G}/members/{BOT}") {
                // The configured role already exists and is below the bot.
                // Its assignment changes while metadata is in flight.
                tokio::time::sleep(Duration::from_millis(100)).await;
                assigned = true;
            }
            if first == format!("GET /api/v10/guilds/{G}/members/{A}") {
                assert!(assigned, "target must be read after the metadata delay");
                body["roles"] = json!(["100000000000000060"]);
            }
            requests.push(first);
            respond(&mut stream, status, &body).await;
        }
        requests
    });
    let ex = ActionExecutor::with_proxy("offline-fixture-token".into(), Some(base)).unwrap();
    let protected = HashSet::from(["100000000000000060".into()]);
    let (summary, records) = run_single(&ex, &protected).await;
    let requests = task.await.unwrap();
    assert!(!summary.aborted);
    assert_eq!(records[0].outcome, RemovalOutcome::Protected);
    assert_eq!(records[0].attempts, 0);
    assert_eq!(ex.requests(), 5);
    assert_eq!(
        requests.last().unwrap(),
        &format!("GET /api/v10/guilds/{G}/members/{A}")
    );
    assert!(requests.iter().all(|p| !p.starts_with("DELETE")));
}

async fn assert_stalled_delete_is_bounded(partial_body: bool) {
    use two_bot_core::MAX_HTTP_TRIES;
    use two_bot_discord::executor::MODERATION_TIMEOUT_MS;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let recorded = Arc::new(Mutex::new(vec![]));
    let requests = recorded.clone();
    let task = tokio::spawn(async move {
        let mut stalled = vec![];
        for _ in 0..MAX_HTTP_TRIES {
            for (expected, status, body) in safety(A, json!([])) {
                let (mut stream, _) = listener.accept().await.unwrap();
                let first = read_request(&mut stream).await;
                assert_eq!(first, expected);
                requests.lock().unwrap().push(first);
                respond(&mut stream, status, &body).await;
            }
            let (mut stream, _) = listener.accept().await.unwrap();
            let first = read_request(&mut stream).await;
            assert_eq!(first, format!("DELETE /api/v10/guilds/{G}/members/{A}"));
            requests.lock().unwrap().push(first);
            if partial_body {
                stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n{")
                    .await
                    .unwrap();
            }
            stalled.push(stream);
        }
        // Keep every stalled exchange open, including the final attempt.
        std::future::pending::<()>().await;
        drop(stalled);
    });
    let ex = ActionExecutor::with_proxy("offline-fixture-token".into(), Some(base)).unwrap();
    let ids = if partial_body {
        vec![A.into(), B.into()]
    } else {
        vec![A.into()]
    };
    let empty = HashSet::new();
    let mut records = vec![];
    let start = Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(45),
        remove_accounts(
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
                // A failed audit after an ambiguous timeout must stop before B.
                if partial_body {
                    Err("disk full".into())
                } else {
                    Ok(())
                }
            },
        ),
    )
    .await;
    task.abort();
    let _ = task.await;
    let result = result.expect("DELETE exchanges must fit the bounded retry budget");
    assert!(
        start.elapsed() >= Duration::from_millis(MODERATION_TIMEOUT_MS * u64::from(MAX_HTTP_TRIES))
    );
    if partial_body {
        assert!(result.is_err());
    } else {
        let summary = result.unwrap();
        assert_eq!(summary.reached, 1);
        assert_eq!(summary.failed, 1);
    }
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].outcome, RemovalOutcome::Failed);
    assert_eq!(records[0].status, None);
    assert_eq!(records[0].attempts, MAX_HTTP_TRIES);
    let requests = recorded.lock().unwrap();
    assert_eq!(requests.len(), 6 * MAX_HTTP_TRIES as usize);
    for attempt in requests.as_chunks::<6>().0 {
        assert_eq!(attempt[4], format!("GET /api/v10/guilds/{G}/members/{A}"));
        assert_eq!(
            attempt[5],
            format!("DELETE /api/v10/guilds/{G}/members/{A}")
        );
    }
    assert!(requests.iter().all(|p| !p.ends_with(B)));
}

#[tokio::test]
async fn stalled_delete_headers_exhaust_budget_and_are_audited() {
    assert_stalled_delete_is_bounded(false).await;
}

#[tokio::test]
async fn stalled_delete_body_exhausts_budget_and_audit_failure_stops_next_target() {
    assert_stalled_delete_is_bounded(true).await;
}

#[tokio::test]
async fn timed_out_delete_then_new_protection_is_failed_and_aborted_without_another_delete() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let mut requests = vec![];
        for (expected, status, body) in safety(A, json!([])) {
            let (mut stream, _) = listener.accept().await.unwrap();
            let first = read_request(&mut stream).await;
            assert_eq!(first, expected);
            requests.push(first);
            respond(&mut stream, status, &body).await;
        }
        let (mut uncertain, _) = listener.accept().await.unwrap();
        let first = read_request(&mut uncertain).await;
        assert_eq!(first, format!("DELETE /api/v10/guilds/{G}/members/{A}"));
        requests.push(first);
        // Leave the first DELETE in flight across the deadline and fresh reads.
        for (expected, status, body) in safety(A, json!(["100000000000000060"])) {
            let (mut stream, _) = listener.accept().await.unwrap();
            let first = read_request(&mut stream).await;
            assert_eq!(first, expected);
            requests.push(first);
            respond(&mut stream, status, &body).await;
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(500), listener.accept())
                .await
                .is_err(),
            "must abort before a second DELETE or next target"
        );
        drop(uncertain);
        requests
    });
    let ex = ActionExecutor::with_proxy("offline-fixture-token".into(), Some(base)).unwrap();
    let empty = HashSet::new();
    let ids = vec![A.into(), B.into()];
    let mut records = vec![];
    let summary = tokio::time::timeout(
        Duration::from_secs(12),
        remove_accounts(
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
        ),
    )
    .await
    .expect("one deadline and fresh protection reads must be bounded")
    .unwrap();
    let requests = task.await.unwrap();
    assert!(summary.aborted);
    assert_eq!(summary.failed, 1);
    assert_eq!(summary.reached, 1);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].outcome, RemovalOutcome::Failed);
    assert_eq!(records[0].attempts, 1);
    assert_eq!(records[0].status, None);
    assert_eq!(
        requests.iter().filter(|p| p.starts_with("DELETE")).count(),
        1
    );
    assert!(requests.iter().all(|p| !p.ends_with(B)));
}

#[tokio::test]
async fn stalled_safety_headers_and_body_time_out_audit_and_abort_without_delete() {
    use two_bot_discord::executor::MODERATION_TIMEOUT_MS;
    for partial_body in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = vec![];
            loop {
                let mut chunk = [0; 4096];
                let n = stream.read(&mut chunk).await.unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&chunk[..n]);
                if bytes.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            assert!(String::from_utf8(bytes).unwrap().starts_with("GET "));
            if partial_body {
                stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n{")
                    .await
                    .unwrap();
            }
            tokio::time::sleep(Duration::from_secs(15)).await;
        });
        let ex = ActionExecutor::with_proxy("offline-fixture-token".into(), Some(base)).unwrap();
        let start = Instant::now();
        let (summary, records) =
            tokio::time::timeout(Duration::from_secs(8), run_single(&ex, &HashSet::new()))
                .await
                .expect("strict read must be bounded");
        assert!(start.elapsed() >= Duration::from_millis(MODERATION_TIMEOUT_MS));
        assert!(summary.aborted);
        assert_eq!(summary.failed, 1);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].outcome, RemovalOutcome::Failed);
        assert_eq!(records[0].attempts, 0);
        assert_eq!(ex.requests(), 1);
        task.abort();
        let _ = task.await;
    }
}

#[tokio::test]
async fn forbidden_read_is_not_missing_and_aborts_without_credential_retry_or_delete() {
    let (ex, requests, task) = mock(vec![step(
        format!("GET /api/v10/guilds/{G}"),
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
    assert_eq!(records[0].outcome, RemovalOutcome::Failed);
    assert_eq!(requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn settled_replay_and_dry_run_make_no_requests_even_with_executor_present() {
    let (ex, requests, task) = mock(vec![]).await;
    task.await.unwrap();
    let ids = vec![A.into()];
    let done = HashSet::from([A.into()]);
    let empty = HashSet::new();
    for mode in [RemovalMode::DryRun, RemovalMode::Execute] {
        let summary = remove_accounts(
            RemovalRun {
                guild: G,
                ids: &ids,
                mode,
                reason: "reviewed",
                run_id: "test",
                done: &done,
                protected: &empty,
            },
            Some(&ex),
            |_| panic!("settled target must not append a new record"),
        )
        .await
        .unwrap();
        assert_eq!(summary.skipped_done, 1);
    }
    let summary = remove_accounts(
        RemovalRun {
            guild: G,
            ids: &ids,
            mode: RemovalMode::DryRun,
            reason: "reviewed",
            run_id: "test",
            done: &empty,
            protected: &empty,
        },
        Some(&ex),
        |r| {
            assert_eq!(r.outcome, RemovalOutcome::WouldKick);
            Ok(())
        },
    )
    .await
    .unwrap();
    assert_eq!(summary.reached, 1);
    assert!(requests.lock().unwrap().is_empty());
    assert_eq!(ex.requests(), 0);
}

#[tokio::test]
async fn audit_failure_after_kick_stops_before_next_member_read() {
    let mut steps = safety(A, json!([]));
    steps.push(step(
        format!("DELETE /api/v10/guilds/{G}/members/{A}"),
        204,
        Value::Null,
    ));
    let (ex, requests, task) = mock(steps).await;
    let ids = vec![A.into(), B.into()];
    let empty = HashSet::new();
    let result = remove_accounts(
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
        |_| Err("disk full".into()),
    )
    .await;
    task.await.unwrap();
    assert!(result.is_err());
    assert_eq!(requests.lock().unwrap().len(), 6);
}

#[cfg(unix)]
#[test]
fn special_audit_files_refuse_before_token_access_and_relative_regular_files_work() {
    use std::os::unix::ffi::OsStrExt;
    let root = std::env::var_os("PAPERCLIP_RUN_SCRATCH_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join(format!(
            "raid-special-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("ids"), A).unwrap();
    let fifo = root.join("fifo");
    let c_path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
    // SAFETY: c_path is NUL-terminated and lives through this local fixture call.
    assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
    for path in [fifo.as_path(), std::path::Path::new("/dev/null")] {
        assert!(FileAudit::open(path, G)
            .err()
            .unwrap()
            .contains("regular file"));
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_raid-remove"))
            .current_dir(&root)
            .args([
                "--guild",
                G,
                "--ids-from",
                "ids",
                "--audit",
                path.to_str().unwrap(),
                "--execute",
                "--expect",
                "1",
                "--reason",
                "reviewed",
            ])
            .env_remove("DISCORD_TOKEN")
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2));
        let err = String::from_utf8(out.stderr).unwrap();
        assert!(err.contains("audit must be a regular file"), "{err}");
        assert!(
            !err.contains("DISCORD_TOKEN"),
            "must refuse before executor construction"
        );
    }
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_raid-remove"))
        .current_dir(&root)
        .args([
            "--guild",
            G,
            "--ids-from",
            "ids",
            "--audit",
            "relative.jsonl",
        ])
        .env_remove("DISCORD_TOKEN")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let audit = FileAudit::open(&root.join("relative.jsonl"), G).unwrap();
    assert!(audit.done.is_empty());
    drop(audit);
    std::fs::remove_file(root.join("relative.jsonl")).unwrap();
    std::fs::remove_file(root.join("ids")).unwrap();
    std::fs::remove_file(fifo).unwrap();
    std::fs::remove_dir(root).unwrap();
}

#[test]
fn malformed_audit_outcomes_and_padded_protected_roles_refuse_before_token_access() {
    let root = std::env::var_os("PAPERCLIP_SCRATCH_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let path = root.join(format!(
        "raid-refusal-{}-{}.jsonl",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let ids = path.with_extension("ids");
    std::fs::write(&ids, A).unwrap();
    let baseline = json!({"v":1,"ts":"2026-09-01T00:00:00Z","run_id":"test","guild_id":G,"member_id":A,"mode":"execute","action":"kick","outcome":"kicked","status":204,"attempts":1});
    for (mode, outcome) in [
        ("execute", "kickd"),
        ("execute", "would_kick"),
        ("dry_run", "kicked"),
    ] {
        let mut damaged = baseline.clone();
        damaged["mode"] = json!(mode);
        damaged["outcome"] = json!(outcome);
        std::fs::write(&path, format!("{damaged}\n")).unwrap();
        assert!(FileAudit::open(&path, G).is_err());
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_raid-remove"))
            .args([
                "--guild",
                G,
                "--ids-from",
                ids.to_str().unwrap(),
                "--audit",
                path.to_str().unwrap(),
                "--execute",
                "--expect",
                "1",
                "--reason",
                "reviewed",
            ])
            .env_remove("DISCORD_TOKEN")
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2));
        let err = String::from_utf8(out.stderr).unwrap();
        assert!(err.contains("audit"), "{err}");
        assert!(
            !err.contains("DISCORD_TOKEN"),
            "must refuse before executor construction"
        );
    }
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_raid-remove"))
        .args([
            "--guild",
            G,
            "--ids-from",
            ids.to_str().unwrap(),
            "--audit",
            path.to_str().unwrap(),
            "--execute",
            "--expect",
            "1",
            "--reason",
            "reviewed",
            "--protected-roles",
            "0100000000000000050",
        ])
        .env_remove("DISCORD_TOKEN")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8(out.stderr)
        .unwrap()
        .contains("invalid Discord snowflake"));
    std::fs::remove_file(&path).unwrap();
    std::fs::remove_file(&ids).unwrap();
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
        let padded = format!("0{}", two_bot_cutover::LIVE_GUILD_ID);
        let out = std::process::Command::new(bin)
            .args(["--guild", &padded])
            .env_remove("TWO_DATABASE_URL")
            .env_remove("DISCORD_TOKEN")
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2));
        assert!(String::from_utf8(out.stderr)
            .unwrap()
            .contains("must be a Discord snowflake"));
        assert!(std::process::Command::new(bin)
            .arg("--help")
            .output()
            .unwrap()
            .status
            .success());
    }
}
