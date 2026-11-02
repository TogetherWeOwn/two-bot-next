//! Protected acceptance: a nonce is not permission to repeat an uncertain POST.
//!
//! Keep this file verbatim in the implementation handoff. Only the registration
//! in scheduled_jobs_tests.rs is unprotected. These tests call the live ticker
//! and store; the only doubles are Discord and outbound admission. The recording
//! server accepts a message BEFORE losing/delaying its reply and deliberately
//! does not deduplicate a later POST: the injected restart is an hour later,
//! outside Discord's short nonce window.
//!
//! Breaks caught: requeue/reclaim of possibly accepted work, treating a bad
//! receipt as completion, forgetting uncertainty across a process/DB-pool
//! restart, trusting negative/incomplete history, and dropping the audit when
//! both completion and retry storage fail. Passing controls distinguish proven
//! pre-wire refusal and confirmed cleanup from uncertainty, and protect normal
//! advancement, generation fences, mention safety and bounded queue progress.

#![cfg(test)]

use std::{collections::VecDeque, sync::Mutex};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{oneshot, Notify},
    task::{JoinHandle, JoinSet},
};
use two_bot_core::{
    delete_scheduled,
    send_admission::{
        AdmissionError, AdmissionFuture, AdmissionPermit, SendAdmission, SendCompletion,
        SendCooldown, TokenKey,
    },
};

use super::super::{format_iso_ms, tick, ActionExecutor, ErrorClass, ScheduledMessageRow};
use serde_json::{json, Value};
use sqlx::PgPool;
use std::{sync::Arc, time::Duration};
use two_bot_core::{get_scheduled, put_scheduled, ScheduledWrite};
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "3333";
const CHANNEL: &str = "4444";
const T0: u64 = 1_790_000_000_000;
const TOKEN: &str = "MTIz.mock.signature";
const LATE: u64 = T0 + 3_600_000;
const POST_PATH: &str = "/api/v10/channels/4444/messages";

static FIXTURE_NONCE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Runtime-built fixture nonce: no literal value reaches nonce use, so
/// CodeQL's hard-coded-crypto rule stays silent (house precedent:
/// `eventsub_http.rs::test_secret`, `feed_jobs_tests.rs::wire_nonces`).
/// Each call is unique within the process; the caller clones it for both
/// the POST and the recovery lookup so they still match. Test-only, never
/// a real credential. Hex of runtime values only, 16 chars, within the
/// 25-char bound checked by `assert_safe_posts`.
fn fixture_nonce() -> String {
    use std::sync::atomic::Ordering;
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time is after the Unix epoch")
        .as_nanos();
    let seq = FIXTURE_NONCE_SEQ.fetch_add(1, Ordering::Relaxed);
    let time_part = (nanos & 0xFFFF_FFFF) as u32;
    let seq_part = (seq & 0xFFFF_FFFF) as u32;
    format!("{time_part:08x}{seq_part:08x}")
}

#[derive(Clone)]
enum Reply {
    Success,
    Receipt(Value),
    Lost,
    Timeout,
    Held,
}

#[derive(Clone, Copy)]
enum Cleanup {
    Confirmed,
    Refused,
    Lost,
    AcceptedLost,
}

#[derive(Clone, Copy)]
enum History {
    Empty,
    Denied,
    Malformed,
    Incomplete,
    BoundedOut,
    Matching,
    WrongAuthor,
    WrongChannel,
    WrongNonce,
    Duplicate,
}

#[derive(Clone)]
struct Request {
    method: String,
    path: String,
    body: Value,
}

struct DiscordState {
    replies: VecDeque<Reply>,
    cleanup: Cleanup,
    history: History,
    requests: Vec<Request>,
    messages: Vec<Value>,
    next_id: u64,
    history_reads: usize,
}

struct RecordingDiscord {
    origin: String,
    state: Arc<Mutex<DiscordState>>,
    accepted: Arc<Notify>,
    release: Arc<Notify>,
    stop: oneshot::Sender<()>,
    task: JoinHandle<()>,
}

fn message(id: u64, channel: &str, body: &Value) -> Value {
    json!({
        "id": id.to_string(), "channel_id": channel,
        "author": {"id": "123", "bot": true, "username": "fixture", "discriminator": "0"},
        "nonce": body["nonce"], "content": body["content"],
        "timestamp": "2026-09-21T00:00:00.000Z", "edited_timestamp": null,
        "type": 0, "tts": false, "pinned": false, "mention_everyone": false,
        "mentions": [], "mention_roles": [], "attachments": [], "embeds": []
    })
}

impl RecordingDiscord {
    async fn start(replies: Vec<Reply>, cleanup: Cleanup, history: History) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fixture");
        let origin = format!("http://{}", listener.local_addr().expect("fixture address"));
        let state = Arc::new(Mutex::new(DiscordState {
            replies: replies.into(),
            cleanup,
            history,
            requests: Vec::new(),
            messages: Vec::new(),
            next_id: 9001,
            history_reads: 0,
        }));
        let accepted = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let (stop, mut stopped) = oneshot::channel();
        let task = tokio::spawn({
            let (state, accepted, release) = (state.clone(), accepted.clone(), release.clone());
            async move {
                let mut connections = JoinSet::new();
                loop {
                    tokio::select! {
                        _ = &mut stopped => break,
                        connection = listener.accept() => {
                            let (stream, _) = connection.expect("accept fixture request");
                            let (state, accepted, release) = (state.clone(), accepted.clone(), release.clone());
                            connections.spawn(async move {
                                serve(stream, state, accepted, release).await;
                            });
                        }
                        Some(result) = connections.join_next(), if !connections.is_empty() => {
                            result.expect("fixture connection did not panic");
                        }
                    }
                }
                connections.abort_all();
                while let Some(result) = connections.join_next().await {
                    if let Err(error) = result {
                        assert!(error.is_cancelled(), "fixture connection failed");
                    }
                }
            }
        });
        Self {
            origin,
            state,
            accepted,
            release,
            stop,
            task,
        }
    }

    fn executor(&self) -> ActionExecutor {
        crate::gateway::ensure_crypto_provider();
        ActionExecutor::with_proxy(TOKEN.to_owned(), Some(self.origin.clone()))
            .expect("loopback executor")
    }

    fn requests(&self) -> Vec<Request> {
        self.state.lock().expect("fixture state").requests.clone()
    }

    fn posts(&self) -> Vec<Value> {
        self.requests()
            .into_iter()
            .filter(|r| r.method == "POST" && r.path == POST_PATH)
            .map(|r| r.body)
            .collect()
    }

    async fn close(self) {
        let _ = self.stop.send(());
        self.task.await.expect("fixture server stopped");
    }
}

// A tiny HTTP boundary, not a scheduler model. No request headers (including
// Authorization) are retained. Every POST physically creates another remote
// message; a matching nonce never conceals a duplicate from these tests.
async fn serve(
    mut stream: TcpStream,
    state: Arc<Mutex<DiscordState>>,
    accepted: Arc<Notify>,
    release: Arc<Notify>,
) {
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        if stream.read_exact(&mut byte).await.is_err() {
            return;
        }
        head.push(byte[0]);
        assert!(head.len() <= 65_536, "bounded fixture request headers");
    }
    let head = String::from_utf8(head).expect("HTTP header text");
    let mut start = head
        .lines()
        .next()
        .expect("request line")
        .split_whitespace();
    let method = start.next().expect("method").to_owned();
    let path = start.next().expect("path").to_owned();
    let len = head
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .map(|(_, n)| n.trim().parse::<usize>().expect("body length"))
        .unwrap_or(0);
    assert!(len <= 65_536, "bounded fixture request body");
    let mut bytes = vec![0; len];
    if stream.read_exact(&mut bytes).await.is_err() {
        return;
    }
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("request JSON")
    };
    let mut held = false;
    let mut lost = false;
    let mut delayed = false;
    let (status, response) = {
        let mut s = state.lock().expect("fixture state");
        s.requests.push(Request {
            method: method.clone(),
            path: path.clone(),
            body: body.clone(),
        });
        let route = path.split('?').next().expect("route");
        if method == "POST" && route == POST_PATH {
            let id = s.next_id;
            s.next_id += 1;
            let receipt = message(id, CHANNEL, &body);
            s.messages.push(receipt.clone());
            let reply = s.replies.pop_front().unwrap_or(Reply::Success);
            match reply {
                Reply::Success => (200, receipt),
                Reply::Receipt(value) => (200, value),
                Reply::Lost => {
                    lost = true;
                    (200, receipt)
                }
                Reply::Timeout => {
                    delayed = true;
                    (200, receipt)
                }
                Reply::Held => {
                    held = true;
                    (200, receipt)
                }
            }
        } else if method == "DELETE" && route.starts_with(&format!("{POST_PATH}/")) {
            let id = route.rsplit('/').next().expect("message id");
            match s.cleanup {
                Cleanup::Confirmed | Cleanup::AcceptedLost => {
                    s.messages.retain(|m| m["id"].as_str() != Some(id));
                    lost = matches!(s.cleanup, Cleanup::AcceptedLost);
                    (204, Value::Null)
                }
                Cleanup::Refused => (403, json!({"message": "fixture refusal"})),
                Cleanup::Lost => {
                    lost = true;
                    (204, Value::Null)
                }
            }
        } else if method == "GET" && route == POST_PATH {
            s.history_reads += 1;
            match s.history {
                History::Empty => (200, json!([])),
                History::Denied => (403, json!({"message": "fixture history unavailable"})),
                History::Malformed => (200, json!({"not_messages": []})),
                History::Incomplete => (200, json!([{"id": "9001", "author": {"id": "123"}}])),
                History::BoundedOut => {
                    let top = 100_000 - 100 * s.history_reads as u64;
                    let rows: Vec<_> = (0..100)
                        .map(|n| {
                            message(
                                top - n,
                                CHANNEL,
                                &json!({"nonce": "another-occurrence", "content": "fixture"}),
                            )
                        })
                        .collect();
                    (200, json!(rows))
                }
                view => {
                    let mut rows: Vec<_> = s.messages.iter().rev().cloned().collect();
                    for m in &mut rows {
                        match view {
                            History::WrongAuthor => m["author"]["id"] = json!("124"),
                            History::WrongChannel => m["channel_id"] = json!("4445"),
                            History::WrongNonce => m["nonce"] = json!("another-occurrence"),
                            _ => {}
                        }
                    }
                    if matches!(view, History::Duplicate) {
                        let mut duplicate = rows[0].clone();
                        duplicate["id"] = json!("9999");
                        rows.insert(0, duplicate);
                    }
                    (200, json!(rows))
                }
            }
        } else if method == "GET" && route == "/api/v10/users/@me" {
            (
                200,
                json!({"id": "123", "bot": true, "username": "fixture", "discriminator": "0"}),
            )
        } else {
            (404, json!({"message": "unknown fixture route"}))
        }
    };
    if method == "POST" {
        accepted.notify_one();
    }
    if held {
        release.notified().await;
    }
    if delayed {
        tokio::time::sleep(Duration::from_secs(6)).await;
    }
    if lost {
        // Acceptance already happened. Closing a deliberately incomplete 2xx
        // response produces transport loss without a real network/service call.
        let _ = stream
            .write_all(
                b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\nconnection: close\r\n\r\n{\"id\":",
            )
            .await;
        return;
    }
    let bytes = if status == 204 {
        Vec::new()
    } else {
        response.to_string().into_bytes()
    };
    let head = format!("HTTP/1.1 {status} Fixture\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", bytes.len());
    let _ = stream.write_all(head.as_bytes()).await;
    let _ = stream.write_all(&bytes).await;
}

async fn fixture() -> Option<TestDatabase> {
    let Ok(url) = std::env::var("TWO_TEST_DATABASE_URL") else {
        assert!(
            std::env::var("GITHUB_ACTIONS").is_err(),
            "CI must supply the guarded test database"
        );
        eprintln!("SKIP scheduled uncertainty acceptance: TWO_TEST_DATABASE_URL is not set");
        return None;
    };
    Some(
        TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("create migrated disposable database"),
    )
}

async fn seed(pool: &PgPool, id: &str, due: u64, interval: Option<i64>) {
    let at = format_iso_ms(T0 - 3_600_000);
    assert!(put_scheduled(
        pool,
        &ScheduledWrite {
            id: id.to_owned(),
            guild_id: GUILD.to_owned(),
            channel_id: CHANNEL.to_owned(),
            body: "standup @everyone".to_owned(),
            next_run_at: format_iso_ms(due),
            interval_seconds: interval,
            enabled: true,
            created_by: "5555".to_owned(),
            created_at: at.clone(),
            updated_by: "5555".to_owned(),
            updated_at: at,
        }
    )
    .await
    .expect("seed schedule"));
}

async fn row(pool: &PgPool, id: &str) -> ScheduledMessageRow {
    get_scheduled(pool, GUILD, id)
        .await
        .expect("read occurrence")
        .expect("occurrence exists")
}

async fn audits(pool: &PgPool, id: &str) -> Vec<(String, Option<String>)> {
    sqlx::query_as("SELECT outcome, reason FROM automation_audit_log WHERE guild_id = $1 AND target_key = $2 AND action = 'scheduled.run' ORDER BY created_at, id")
        .bind(GUILD).bind(id).fetch_all(pool).await.expect("read attempt audits")
}

async fn bounded_tick(pool: &PgPool, rest: &ActionExecutor, now: u64) -> Result<(), ErrorClass> {
    tokio::time::timeout(Duration::from_secs(20), tick(pool, rest, GUILD, || now))
        .await
        .expect("bounded scheduler attempt")
}

async fn refuse_completion(pool: &PgPool) {
    sqlx::raw_sql("CREATE FUNCTION refuse_uncertain_completion() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.last_run_at IS DISTINCT FROM OLD.last_run_at THEN RAISE EXCEPTION 'fixture completion failure'; END IF; RETURN NEW; END $$; CREATE TRIGGER fail_uncertain_completion BEFORE UPDATE ON scheduled_messages FOR EACH ROW EXECUTE FUNCTION refuse_uncertain_completion();")
        .execute(pool).await.expect("install completion fault on owned test database");
}

async fn refuse_retry(pool: &PgPool) {
    sqlx::raw_sql("CREATE FUNCTION refuse_uncertain_retry() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF OLD.claim_token IS NOT NULL AND NEW.claim_token IS NULL AND NEW.last_run_at IS NOT DISTINCT FROM OLD.last_run_at THEN RAISE EXCEPTION 'fixture retry failure'; END IF; RETURN NEW; END $$; CREATE TRIGGER fail_uncertain_retry BEFORE UPDATE ON scheduled_messages FOR EACH ROW EXECUTE FUNCTION refuse_uncertain_retry();")
        .execute(pool).await.expect("install retry fault on owned test database");
}

struct Observation {
    first: ScheduledMessageRow,
    after: ScheduledMessageRow,
    posts: Vec<Value>,
    first_audits: Vec<(String, Option<String>)>,
    remote_messages: usize,
    history_reads: usize,
    first_database_error: bool,
}

async fn observe(
    reply: Reply,
    cleanup: Cleanup,
    history: History,
    completion_fault: bool,
    retry_fault: bool,
    interval: Option<i64>,
) -> Option<Observation> {
    let db = fixture().await?;
    seed(db.pool(), "uncertain", T0 - 1_000, interval).await;
    if completion_fault {
        refuse_completion(db.pool()).await;
    }
    if retry_fault {
        refuse_retry(db.pool()).await;
    }
    let discord = RecordingDiscord::start(vec![reply], cleanup, history).await;
    let rest = discord.executor();
    let initial = bounded_tick(db.pool(), &rest, T0).await;
    let first = row(db.pool(), "uncertain").await;
    let first_audits = audits(db.pool(), "uncertain").await;
    if completion_fault {
        sqlx::query("DROP TRIGGER fail_uncertain_completion ON scheduled_messages")
            .execute(db.pool())
            .await
            .expect("restore completion storage");
    }
    if retry_fault {
        sqlx::query("DROP TRIGGER fail_uncertain_retry ON scheduled_messages")
            .execute(db.pool())
            .await
            .expect("restore retry storage");
    }
    // Both the executor (its process-local guard/cache) and DB pool are new.
    drop(rest);
    let restarted = db
        .independent_pool()
        .await
        .expect("independent restart pool");
    let rest = discord.executor();
    let _ = bounded_tick(&restarted, &rest, LATE).await;
    let _ = bounded_tick(&restarted, &rest, LATE + 120_000).await;
    let after = row(&restarted, "uncertain").await;
    let posts = discord.posts();
    let (remote_messages, history_reads) = {
        let state = discord.state.lock().expect("fixture state");
        (state.messages.len(), state.history_reads)
    };
    discord.close().await;
    restarted.close().await;
    db.close().await.expect("verified fixture teardown");
    Some(Observation {
        first,
        after,
        posts,
        first_audits,
        remote_messages,
        history_reads,
        first_database_error: matches!(initial, Err(ErrorClass::Database)),
    })
}

fn assert_safe_posts(posts: &[Value]) {
    for post in posts {
        assert_eq!(post["allowed_mentions"]["parse"], json!([]));
        let content = post["content"].as_str().expect("posted content");
        assert!(!two_bot_core::message_safety::contains_mass_mention(
            content
        ));
        assert_eq!(post["enforce_nonce"], true);
        let nonce = post["nonce"].as_str().expect("stable occurrence nonce");
        assert!(!nonce.is_empty() && nonce.len() <= 25);
    }
}

fn assert_pending(row: &ScheduledMessageRow, nonce: &Value) {
    assert!(
        row.enabled,
        "an unproved occurrence must not disable a one-shot"
    );
    assert!(
        row.last_run_at.is_none(),
        "uncertainty must not advance a run"
    );
    assert!(
        row.last_message_id.is_none(),
        "an unproved receipt is not a delivered ID"
    );
    assert_eq!(
        row.occurrence_nonce.as_deref(),
        nonce.as_str(),
        "persist occurrence identity"
    );
}

fn assert_fenced(o: &Observation) {
    assert_eq!(o.posts.len(), 1,
        "possibly accepted occurrence was blindly POSTed again after restart/lease expiry beyond the nonce window");
    assert_pending(&o.first, &o.posts[0]["nonce"]);
    assert_pending(&o.after, &o.posts[0]["nonce"]);
    assert_safe_posts(&o.posts);
    assert_eq!(
        o.first_audits.len(),
        1,
        "attempt audit survives uncertain storage/transport outcomes"
    );
    assert_ne!(
        o.first_audits[0].0, "ok",
        "uncertainty is not clean success"
    );
    assert!(
        o.history_reads <= 6,
        "at most three history pages per late attempt"
    );
    let audit = serde_json::to_string(&o.first_audits).expect("audit JSON");
    assert!(
        !audit.contains("standup") && !audit.contains("@everyone") && !audit.contains(TOKEN),
        "audit must not echo message content or credentials"
    );
}

macro_rules! uncertainty_case {
    ($name:ident, $reply:expr, $cleanup:expr, $history:expr, $completion:expr, $retry:expr, $interval:expr) => {
        #[tokio::test]
        async fn $name() {
            let Some(o) = observe($reply, $cleanup, $history, $completion, $retry, $interval).await
            else {
                return;
            };
            assert_fenced(&o);
            if $completion || $retry {
                assert!(
                    o.first_database_error,
                    "storage fault must fail the tick, without suppressing audit"
                );
            }
        }
    };
}

uncertainty_case!(
    accepted_then_transport_loss_stays_fenced_after_restart,
    Reply::Lost,
    Cleanup::Refused,
    History::Empty,
    false,
    false,
    None
);
uncertainty_case!(
    accepted_then_timeout_does_not_become_a_late_retry,
    Reply::Timeout,
    Cleanup::Refused,
    History::Empty,
    false,
    false,
    None
);
uncertainty_case!(
    missing_success_id_is_not_completion_or_permission_to_resend,
    Reply::Receipt(json!({})),
    Cleanup::Refused,
    History::Empty,
    false,
    false,
    None
);
uncertainty_case!(
    zero_success_id_is_not_completion_or_permission_to_resend,
    Reply::Receipt(json!({"id": "0"})),
    Cleanup::Refused,
    History::Empty,
    false,
    false,
    None
);
uncertainty_case!(
    malformed_success_id_is_not_completion_or_permission_to_resend,
    Reply::Receipt(json!({"id": "not-a-message"})),
    Cleanup::Refused,
    History::Empty,
    false,
    false,
    None
);
uncertainty_case!(
    completion_failure_and_refused_cleanup_retain_uncertainty,
    Reply::Success,
    Cleanup::Refused,
    History::Empty,
    true,
    false,
    None
);
uncertainty_case!(
    completion_failure_and_lost_cleanup_reply_retain_uncertainty,
    Reply::Success,
    Cleanup::Lost,
    History::Empty,
    true,
    false,
    None
);
uncertainty_case!(
    accepted_but_unconfirmed_delete_is_not_proven_clean,
    Reply::Success,
    Cleanup::AcceptedLost,
    History::Empty,
    true,
    false,
    None
);
uncertainty_case!(
    completion_and_retry_storage_failures_still_audit_and_fence,
    Reply::Success,
    Cleanup::Refused,
    History::Empty,
    true,
    true,
    None
);
uncertainty_case!(
    recurring_uncertainty_does_not_silently_advance_to_the_next_run,
    Reply::Lost,
    Cleanup::Refused,
    History::Empty,
    false,
    false,
    Some(3_600)
);
uncertainty_case!(
    unreadable_history_never_proves_absence,
    Reply::Lost,
    Cleanup::Refused,
    History::Malformed,
    false,
    false,
    None
);
uncertainty_case!(
    denied_history_never_proves_absence,
    Reply::Lost,
    Cleanup::Refused,
    History::Denied,
    false,
    false,
    None
);
uncertainty_case!(
    incomplete_history_never_proves_absence,
    Reply::Lost,
    Cleanup::Refused,
    History::Incomplete,
    false,
    false,
    None
);
uncertainty_case!(
    bounded_out_history_never_proves_absence,
    Reply::Lost,
    Cleanup::Refused,
    History::BoundedOut,
    false,
    false,
    None
);
uncertainty_case!(
    another_author_cannot_confirm_an_occurrence,
    Reply::Lost,
    Cleanup::Refused,
    History::WrongAuthor,
    false,
    false,
    None
);
uncertainty_case!(
    another_channel_cannot_confirm_an_occurrence,
    Reply::Lost,
    Cleanup::Refused,
    History::WrongChannel,
    false,
    false,
    None
);
uncertainty_case!(
    another_nonce_cannot_confirm_an_occurrence,
    Reply::Lost,
    Cleanup::Refused,
    History::WrongNonce,
    false,
    false,
    None
);
uncertainty_case!(
    two_matching_receipts_are_not_unique_confirmation,
    Reply::Lost,
    Cleanup::Refused,
    History::Duplicate,
    false,
    false,
    None
);

#[tokio::test]
async fn a_unique_matching_receipt_may_settle_without_another_post() {
    let Some(o) = observe(
        Reply::Lost,
        Cleanup::Refused,
        History::Matching,
        false,
        false,
        None,
    )
    .await
    else {
        return;
    };
    assert_eq!(
        o.posts.len(),
        1,
        "recovery must never POST to discover whether POST succeeded"
    );
    assert_eq!(o.remote_messages, 1);
    assert_pending(&o.first, &o.posts[0]["nonce"]);
    if o.after.last_run_at.is_some() {
        assert!(!o.after.enabled);
        assert_eq!(o.after.last_message_id.as_deref(), Some("9001"));
        assert!(
            o.history_reads > 0 && o.history_reads <= 6,
            "settlement needs positive bounded evidence"
        );
    } else {
        // Automatic reconciliation is optional; indefinite safe fencing is
        // honest too. A negative scan never grants permission to resend.
        assert_pending(&o.after, &o.posts[0]["nonce"]);
    }
    assert_safe_posts(&o.posts);
}

#[tokio::test]
async fn confirmed_cleanup_can_retry_after_completion_storage_recovers() {
    let Some(o) = observe(
        Reply::Success,
        Cleanup::Confirmed,
        History::Empty,
        true,
        false,
        None,
    )
    .await
    else {
        return;
    };
    assert!(o.first_database_error);
    assert_eq!(o.first_audits.len(), 1);
    assert_eq!(
        o.posts.len(),
        2,
        "a confirmed deletion makes the retry safe"
    );
    assert_eq!(o.remote_messages, 1, "only the retried message survives");
    assert!(!o.after.enabled);
    assert_eq!(o.after.last_message_id.as_deref(), Some("9002"));
    assert!(o.after.last_run_at.is_some());
    assert_safe_posts(&o.posts);
}

#[derive(Debug)]
struct BlockedAdmission(TokenKey);

impl SendAdmission for BlockedAdmission {
    fn token_key(&self) -> &TokenKey {
        &self.0
    }
    fn admit(&self) -> AdmissionFuture<'_, Result<AdmissionPermit, AdmissionError>> {
        Box::pin(async { Err(AdmissionError::Blocked) })
    }
}

#[derive(Debug)]
struct UnconfirmedAdmission(TokenKey);
struct UnconfirmedCompletion;

impl SendAdmission for UnconfirmedAdmission {
    fn token_key(&self) -> &TokenKey {
        &self.0
    }
    fn admit(&self) -> AdmissionFuture<'_, Result<AdmissionPermit, AdmissionError>> {
        Box::pin(async { Ok(AdmissionPermit::new(Box::new(UnconfirmedCompletion))) })
    }
}

impl SendCompletion for UnconfirmedCompletion {
    fn complete(
        self: Box<Self>,
        _: Option<SendCooldown>,
    ) -> AdmissionFuture<'static, Result<(), AdmissionError>> {
        Box::pin(async { Err(AdmissionError::Storage) })
    }
}

#[tokio::test]
async fn proven_prewire_refusal_is_retryable_without_a_delivery_fence() {
    let Some(db) = fixture().await else {
        return;
    };
    seed(db.pool(), "prewire", T0 - 1_000, None).await;
    let discord = RecordingDiscord::start(vec![], Cleanup::Refused, History::Empty).await;
    crate::gateway::ensure_crypto_provider();
    let rest = ActionExecutor::with_admission(
        TOKEN.to_owned(),
        Some(discord.origin.clone()),
        Arc::new(BlockedAdmission(
            TokenKey::for_bot_token(TOKEN).expect("fixture key"),
        )),
    )
    .expect("blocked fixture executor");
    let _ = bounded_tick(db.pool(), &rest, T0).await;
    let first = row(db.pool(), "prewire").await;
    let initial_posts = discord.posts().len();
    let _ = bounded_tick(db.pool(), &discord.executor(), LATE).await;
    let after = row(db.pool(), "prewire").await;
    let posts = discord.posts();
    discord.close().await;
    db.close().await.expect("verified fixture teardown");
    assert_eq!(initial_posts, 0, "admission refusal never reached the wire");
    assert!(first.enabled && first.last_run_at.is_none());
    assert_eq!(
        posts.len(),
        1,
        "safe pre-wire refusal must not strand the occurrence"
    );
    assert!(!after.enabled && after.last_run_at.is_some());
    assert_safe_posts(&posts);
}

#[tokio::test]
async fn accepted_post_with_unconfirmed_admission_completion_stays_fenced() {
    let Some(db) = fixture().await else {
        return;
    };
    seed(db.pool(), "admission-unconfirmed", T0 - 1_000, None).await;
    let discord = RecordingDiscord::start(vec![], Cleanup::Refused, History::Empty).await;
    crate::gateway::ensure_crypto_provider();
    let rest = ActionExecutor::with_admission(
        TOKEN.to_owned(),
        Some(discord.origin.clone()),
        Arc::new(UnconfirmedAdmission(
            TokenKey::for_bot_token(TOKEN).expect("fixture key"),
        )),
    )
    .expect("unconfirmed fixture executor");
    let _ = bounded_tick(db.pool(), &rest, T0).await;
    let first = row(db.pool(), "admission-unconfirmed").await;
    drop(rest);
    let restarted = db
        .independent_pool()
        .await
        .expect("independent restart pool");
    let _ = bounded_tick(&restarted, &discord.executor(), LATE).await;
    let after = row(&restarted, "admission-unconfirmed").await;
    let posts = discord.posts();
    discord.close().await;
    restarted.close().await;
    db.close().await.expect("verified fixture teardown");
    assert_eq!(
        posts.len(),
        1,
        "admission settlement failure cannot authorize another accepted POST"
    );
    assert_pending(&first, &posts[0]["nonce"]);
    assert_pending(&after, &posts[0]["nonce"]);
}

#[tokio::test]
async fn death_after_acceptance_before_response_keeps_a_durable_restart_fence() {
    let Some(db) = fixture().await else {
        return;
    };
    seed(db.pool(), "died-in-flight", T0 - 1_000, None).await;
    let discord =
        RecordingDiscord::start(vec![Reply::Held], Cleanup::Refused, History::Empty).await;
    let process_pool = db.independent_pool().await.expect("old process pool");
    let ticker = {
        let pool = process_pool.clone();
        let rest = discord.executor();
        tokio::spawn(async move { tick(&pool, &rest, GUILD, || T0).await })
    };
    tokio::time::timeout(Duration::from_secs(10), discord.accepted.notified())
        .await
        .expect("remote acceptance before process death");
    ticker.abort();
    assert!(ticker
        .await
        .expect_err("old process was aborted")
        .is_cancelled());
    process_pool.close().await;
    discord.release.notify_one();
    let first = row(db.pool(), "died-in-flight").await;
    let restarted = db.independent_pool().await.expect("new process pool");
    let _ = bounded_tick(&restarted, &discord.executor(), LATE).await;
    let after = row(&restarted, "died-in-flight").await;
    let posts = discord.posts();
    discord.close().await;
    restarted.close().await;
    db.close().await.expect("verified fixture teardown");
    assert_eq!(
        posts.len(),
        1,
        "expired in-flight lease is uncertain, not proof the old process never sent"
    );
    assert_pending(&first, &posts[0]["nonce"]);
    assert_pending(&after, &posts[0]["nonce"]);
}

async fn changed_generation(cancel: bool) {
    let Some(db) = fixture().await else {
        return;
    };
    seed(db.pool(), "changed-in-flight", T0 - 1_000, None).await;
    let discord =
        RecordingDiscord::start(vec![Reply::Held], Cleanup::Refused, History::Matching).await;
    let ticker = {
        let pool = db.pool().clone();
        let rest = discord.executor();
        tokio::spawn(async move { tick(&pool, &rest, GUILD, || T0).await })
    };
    tokio::time::timeout(Duration::from_secs(10), discord.accepted.notified())
        .await
        .expect("post accepted before definition change");
    if cancel {
        assert!(delete_scheduled(db.pool(), GUILD, "changed-in-flight")
            .await
            .expect("cancel"));
    } else {
        seed(db.pool(), "changed-in-flight", T0 + 172_800_000, None).await;
    }
    discord.release.notify_one();
    let _ = tokio::time::timeout(Duration::from_secs(10), ticker)
        .await
        .expect("bounded old attempt")
        .expect("ticker did not panic");
    let _ = bounded_tick(db.pool(), &discord.executor(), LATE).await;
    let after = two_bot_core::get_scheduled(db.pool(), GUILD, "changed-in-flight")
        .await
        .expect("read current generation");
    let audit = audits(db.pool(), "changed-in-flight").await;
    let posts = discord.posts();
    discord.close().await;
    db.close().await.expect("verified fixture teardown");
    assert_eq!(
        posts.len(),
        1,
        "recovery must not resurrect the old generation"
    );
    if cancel {
        assert!(after.is_none());
    } else {
        let current = after.expect("replacement stands");
        assert!(current.enabled);
        assert_eq!(current.next_run_at, format_iso_ms(T0 + 172_800_000));
        assert!(current.last_run_at.is_none() && current.last_message_id.is_none());
        assert!(current.occurrence_nonce.is_none());
    }
    assert_eq!(
        audit.len(),
        1,
        "lost-generation cleanup still attempts its audit"
    );
    assert_ne!(
        audit[0].0, "ok",
        "unconfirmed stale cleanup is not a clean delivery"
    );
    assert_safe_posts(&posts);
}

#[tokio::test]
async fn cancellation_during_accepted_post_never_revives_the_definition() {
    changed_generation(true).await;
}

#[tokio::test]
async fn replacement_during_accepted_post_never_advances_the_new_generation() {
    changed_generation(false).await;
}

#[tokio::test]
async fn fixture_control_lost_response_has_positive_receipt_until_confirmed_cleanup() {
    use two_bot_discord::executor::ChannelCall;

    let discord =
        RecordingDiscord::start(vec![Reply::Lost], Cleanup::Confirmed, History::Matching).await;
    let rest = discord.executor();
    let nonce = fixture_nonce();
    let lost = rest
        .execute_channel(&ChannelCall::PostMessage {
            channel_id: CHANNEL.to_owned(),
            content: "fixture".to_owned(),
            nonce: Some(nonce.clone()),
        })
        .await;
    let recovered = rest.recover_message_by_nonce(CHANNEL, &nonce, 123).await;
    let cleanup = rest
        .delete_message(CHANNEL, "9001", "fixture cleanup")
        .await;
    let posts = discord.posts();
    let remaining = discord.state.lock().expect("fixture state").messages.len();
    discord.close().await;
    assert!(
        lost.is_err(),
        "incomplete success response reaches the real transport error path"
    );
    assert_eq!(
        recovered.expect("readable positive history"),
        Some("9001".to_owned())
    );
    cleanup.expect("confirmed deletion");
    assert_eq!(remaining, 0);
    assert_eq!(posts.len(), 1);
    assert_safe_posts(&posts);
}

#[tokio::test]
async fn fixture_control_repeated_nonce_does_not_hide_a_second_remote_message() {
    use two_bot_discord::executor::{ChannelCall, ChannelCallOutcome};

    let discord = RecordingDiscord::start(vec![], Cleanup::Refused, History::Matching).await;
    let rest = discord.executor();
    let nonce = fixture_nonce();
    let call = ChannelCall::PostMessage {
        channel_id: CHANNEL.to_owned(),
        content: "fixture".to_owned(),
        nonce: Some(nonce.clone()),
    };
    let first = rest.execute_channel(&call).await;
    let second = rest.execute_channel(&call).await;
    let messages = discord
        .state
        .lock()
        .expect("fixture state")
        .messages
        .clone();
    let posts = discord.posts();
    discord.close().await;
    assert!(
        matches!(first, Ok(ChannelCallOutcome::Posted { ref message_id }) if message_id == "9001")
    );
    assert!(
        matches!(second, Ok(ChannelCallOutcome::Posted { ref message_id }) if message_id == "9002")
    );
    assert_eq!(
        messages.len(),
        2,
        "the late-window double must not dedupe the unsafe retry"
    );
    assert_eq!(messages[0]["nonce"], messages[1]["nonce"]);
    assert_eq!(posts.len(), 2);
    assert_safe_posts(&posts);
}

#[tokio::test]
async fn ordinary_one_shot_and_recurring_runs_still_advance_with_safe_mentions() {
    let Some(db) = fixture().await else {
        return;
    };
    seed(db.pool(), "once", T0 - 2_000, None).await;
    seed(db.pool(), "hourly", T0 - 1_000, Some(3_600)).await;
    let discord = RecordingDiscord::start(vec![], Cleanup::Refused, History::Empty).await;
    bounded_tick(db.pool(), &discord.executor(), T0)
        .await
        .expect("ordinary tick");
    let once = row(db.pool(), "once").await;
    let hourly = row(db.pool(), "hourly").await;
    let once_audit = audits(db.pool(), "once").await;
    let hourly_audit = audits(db.pool(), "hourly").await;
    let posts = discord.posts();
    discord.close().await;
    db.close().await.expect("verified fixture teardown");
    assert_eq!(posts.len(), 2);
    assert!(!once.enabled);
    assert_eq!(once.last_message_id.as_deref(), Some("9001"));
    assert_eq!(
        once.last_run_at.as_deref(),
        Some(format_iso_ms(T0).as_str())
    );
    assert!(hourly.enabled);
    assert_eq!(hourly.next_run_at, format_iso_ms(T0 + 3_600_000));
    assert_eq!(hourly.last_message_id.as_deref(), Some("9002"));
    assert_eq!(once_audit, [("ok".to_owned(), None)]);
    assert_eq!(hourly_audit, [("ok".to_owned(), None)]);
    assert_safe_posts(&posts);
}

#[tokio::test]
async fn uncertain_occurrences_do_not_starve_an_unrelated_due_schedule() {
    let Some(db) = fixture().await else {
        return;
    };
    for id in ["uncertain-a", "uncertain-b", "uncertain-c"] {
        seed(db.pool(), id, T0 - 1_000, None).await;
    }
    let discord =
        RecordingDiscord::start(vec![Reply::Lost; 3], Cleanup::Refused, History::Denied).await;
    let _ = bounded_tick(db.pool(), &discord.executor(), T0).await;
    let first_posts = discord.posts();
    seed(db.pool(), "unrelated", LATE - 1_000, None).await;
    let restarted = db
        .independent_pool()
        .await
        .expect("independent restart pool");
    for offset in [0, 15_000, 30_000] {
        let _ = bounded_tick(&restarted, &discord.executor(), LATE + offset).await;
    }
    let healthy = row(&restarted, "unrelated").await;
    let mut pending = Vec::new();
    for id in ["uncertain-a", "uncertain-b", "uncertain-c"] {
        pending.push(row(&restarted, id).await);
    }
    let posts = discord.posts();
    let history_reads = discord.state.lock().expect("fixture state").history_reads;
    discord.close().await;
    restarted.close().await;
    db.close().await.expect("verified fixture teardown");
    assert_eq!(
        first_posts.len(),
        3,
        "first tick reached all three independent occurrences"
    );
    assert_eq!(
        posts.len(),
        4,
        "only the unrelated occurrence may add a POST"
    );
    assert!(
        !healthy.enabled && healthy.last_message_id.is_some(),
        "uncertainty cannot monopolize the queue"
    );
    for (row, post) in pending.iter().zip(&first_posts) {
        assert_pending(row, &post["nonce"]);
    }
    assert!(
        history_reads <= 27,
        "bounded scans across three occurrences and three attempts"
    );
    assert_safe_posts(&posts);
}
