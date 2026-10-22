//! Protected acceptance corpus: uncertain sticky delivery must not authorize a
//! fresh send. The real runtime, HTTP adapter, migrations and store stay live.
//! See sticky_uncertain_acceptance_manifest.md for the handoff contract.
#![cfg(test)]

use super::{
    executor_at, gates, message, option, read_rest_request, slash, ActivityOutcome,
    CommandOptionValue, CommandRuntime, CHANNEL, CHANNEL_S, GUILD, GUILD_S,
};
use futures_util::FutureExt;
use sqlx::PgPool;
use std::collections::BTreeMap;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::sync::Notify;
use tokio::task::{JoinHandle, JoinSet};
use two_bot_core::funnel::now_millis_for_test;
use two_bot_core::send_admission::{
    AdmissionError, AdmissionFuture, AdmissionPermit, SendAdmission, SendCompletion, SendCooldown,
    TokenKey,
};
use two_bot_core::sticky::{store, PutSticky, RemoveOutcome};
use two_bot_discord::ActionExecutor;
use two_bot_testsupport::TestDatabase;

const OLD: &str = "8000000000000000001";
const ORPHAN: &str = "9000000000000000100";
const BODY: &str = "synthetic sticky body must not appear in diagnostics";
const MESSAGES: &str = "/api/v10/channels/3333/messages";

#[derive(Clone, Copy)]
enum FirstPost {
    Confirm,
    Body(&'static str),
    LostResponse,
    Held,
    Rejected,
}

#[derive(Clone, Copy)]
enum Cleanup {
    Deleted,
    AlreadyAbsent,
    Forbidden,
    LostResponse,
}

#[derive(Default)]
struct DiscordState {
    posts: Vec<serde_json::Value>,
    deletes: Vec<String>,
    live: BTreeMap<String, serde_json::Value>,
}

struct Discord {
    origin: String,
    state: Arc<Mutex<DiscordState>>,
    accepted: Arc<Notify>,
    finish: Arc<Notify>,
    task: JoinHandle<()>,
}

impl Drop for Discord {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Discord {
    async fn start(first: FirstPost, cleanup: Cleanup) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback double");
        let origin = format!(
            "http://{}",
            listener.local_addr().expect("loopback address")
        );
        let state = Arc::new(Mutex::new(DiscordState::default()));
        state.lock().expect("state").live.insert(
            OLD.into(),
            serde_json::json!({"id": OLD, "content": "previous sticky"}),
        );
        let accepted = Arc::new(Notify::new());
        let finish = Arc::new(Notify::new());
        let (shared, entered, release) = (state.clone(), accepted.clone(), finish.clone());
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    result = listener.accept() => {
                        let (mut stream, _) = result.expect("accept loopback connection");
                        let (shared, entered, release) =
                            (shared.clone(), entered.clone(), release.clone());
                        connections.spawn(async move {
                            let Some((method, path, body)) = read_rest_request(&mut stream).await else {
                                return;
                            };
                            let (status, response, hold, lose) = if method == "POST" && path == MESSAGES {
                                let body: serde_json::Value = serde_json::from_slice(&body)
                                    .expect("runtime sends JSON");
                                let (index, id) = {
                                    let mut state = shared.lock().expect("state");
                                    let index = state.posts.len();
                                    let id = (9_000_000_000_000_000_100u64 + index as u64).to_string();
                                    state.posts.push(body.clone());
                                    // Model acceptance independently of the response the client sees.
                                    if index != 0 || !matches!(first, FirstPost::Rejected) {
                                        state.live.insert(id.clone(), body);
                                    }
                                    (index, id)
                                };
                                if index == 0 {
                                    entered.notify_one();
                                    match first {
                                        FirstPost::Confirm => (200, format!("{{\"id\":\"{id}\"}}"), false, false),
                                        FirstPost::Body(body) => (200, body.into(), false, false),
                                        FirstPost::LostResponse => (200, String::new(), false, true),
                                        FirstPost::Held => (200, format!("{{\"id\":\"{id}\"}}"), true, false),
                                        FirstPost::Rejected => (403, "{}".into(), false, false),
                                    }
                                } else {
                                    (200, format!("{{\"id\":\"{id}\"}}"), false, false)
                                }
                            } else if method == "DELETE" && path.starts_with(MESSAGES) {
                                let id = path.rsplit('/').next().expect("message id");
                                let mut state = shared.lock().expect("state");
                                state.deletes.push(id.into());
                                match cleanup {
                                    Cleanup::Deleted | Cleanup::AlreadyAbsent => {
                                        state.live.remove(id);
                                        let status = if matches!(cleanup, Cleanup::Deleted) { 204 } else { 404 };
                                        (status, String::new(), false, false)
                                    }
                                    Cleanup::Forbidden => (403, "{}".into(), false, false),
                                    Cleanup::LostResponse => (200, String::new(), false, true),
                                }
                            } else {
                                (200, "{\"id\":\"99\"}".into(), false, false)
                            };
                            if hold {
                                release.notified().await;
                            }
                            if !lose {
                                let head = format!(
                                    "HTTP/1.1 {status} Fixture\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                                    response.len()
                                );
                                let _ = stream.write_all(head.as_bytes()).await;
                                let _ = stream.write_all(response.as_bytes()).await;
                            }
                            let _ = stream.shutdown().await;
                        });
                    }
                    _ = connections.join_next(), if !connections.is_empty() => {}
                }
            }
        });
        Self {
            origin,
            state,
            accepted,
            finish,
            task,
        }
    }

    fn runtime(&self, pool: PgPool, automations: bool) -> Arc<CommandRuntime> {
        // The real HTTP executor bounds a held response; no synthetic outcome injection.
        let executor = ActionExecutor::with_proxy_and_moderation_timeout(
            "test-token".into(),
            Some(self.origin.clone()),
            Duration::from_millis(500),
        )
        .expect("loopback executor");
        CommandRuntime::new(
            pool,
            executor,
            super::router_with_commands(gates(automations, true)),
            GUILD,
            automations,
        )
    }

    async fn entered(&self) {
        tokio::time::timeout(Duration::from_secs(10), self.accepted.notified())
            .await
            .expect("POST reached Discord before race");
    }

    fn posts(&self) -> Vec<serde_json::Value> {
        self.state.lock().expect("state").posts.clone()
    }

    fn live_ids(&self) -> Vec<String> {
        self.state
            .lock()
            .expect("state")
            .live
            .keys()
            .cloned()
            .collect()
    }
}

// Always tear down the owned disposable database, including behavioral RED.
async fn fixture(
    run: impl for<'a> FnOnce(&'a TestDatabase) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>,
) {
    let Ok(url) = std::env::var("TWO_TEST_DATABASE_URL") else {
        assert_ne!(
            std::env::var("GITHUB_ACTIONS").as_deref(),
            Ok("true"),
            "CI must configure sticky uncertainty acceptance: TWO_TEST_DATABASE_URL is not configured"
        );
        eprintln!("SKIP sticky uncertainty acceptance: TWO_TEST_DATABASE_URL is not configured");
        return;
    };
    let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("guarded disposable test database");
    store::put_sticky(
        db.pool(),
        &PutSticky {
            guild_id: GUILD_S,
            channel_id: CHANNEL_S,
            body: BODY,
            debounce_seconds: 5,
            enabled: true,
            actor_id: "77",
            now_ms: now_millis_for_test(),
        },
    )
    .await
    .expect("seed sticky");
    sqlx::query("UPDATE sticky_messages SET last_message_id = $1, last_posted_at = clock_timestamp() - interval '2 minutes'")
        .bind(OLD).execute(db.pool()).await.expect("seed due previous sticky");
    let result = AssertUnwindSafe(run(&db)).catch_unwind().await;
    db.close()
        .await
        .expect("drop owned disposable database even on RED");
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[test]
fn missing_database_url_fails_in_ci_but_explicitly_skips_locally() {
    // Exercise the real fixture through an existing test in isolated processes;
    // changing the environment of the concurrently running test binary is unsafe.
    let module = module_path!()
        .split_once("::")
        .expect("nested test module")
        .1;
    let target =
        format!("{module}::disabled_automation_and_ineligible_activity_touch_no_delivery_state");
    for (ci, succeeds, diagnostic) in [
        (
            "true",
            false,
            "CI must configure sticky uncertainty acceptance",
        ),
        ("false", true, "SKIP sticky uncertainty acceptance"),
    ] {
        let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args(["--exact", &target, "--nocapture", "--test-threads=1"])
            .env_remove("TWO_TEST_DATABASE_URL")
            .env("GITHUB_ACTIONS", ci)
            .output()
            .expect("isolated fixture invocation");
        let text = format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            text.contains("running 1 test"),
            "fixture must execute: {text}"
        );
        assert_eq!(
            output.status.success(),
            succeeds,
            "missing database CI={ci} must not silently pass: {text}"
        );
        assert!(text.contains(diagnostic), "explicit diagnosis: {text}");
    }
}

async fn row(pool: &PgPool) -> Option<serde_json::Value> {
    sqlx::query_scalar(
        "SELECT to_jsonb(s) FROM sticky_messages s WHERE guild_id = $1 AND channel_id = $2",
    )
    .bind(GUILD_S)
    .bind(CHANNEL_S)
    .fetch_optional(pool)
    .await
    .expect("persisted sticky")
}

async fn activity(runtime: &CommandRuntime, id: u64) -> ActivityOutcome {
    tokio::time::timeout(
        Duration::from_secs(15),
        runtime.on_message(&message(id, CHANNEL, false, Some(GUILD))),
    )
    .await
    .expect("runtime sticky attempt terminates")
}

fn assert_one_attempt(discord: &Discord) {
    let posts = discord.posts();
    assert_eq!(
        posts.len(),
        1,
        "uncertainty must not authorize a second POST/nonce: {posts:?}"
    );
    assert_eq!(posts[0]["enforce_nonce"], true);
    assert!(
        posts[0]["nonce"].as_str().is_some_and(|s| !s.is_empty())
            || posts[0]["nonce"].as_u64().is_some(),
        "wire attempt has a usable nonce"
    );
    assert_eq!(posts[0]["content"], BODY);
    assert_eq!(posts[0]["allowed_mentions"]["parse"], serde_json::json!([]));
}

async fn assert_fenced(pool: &PgPool) {
    let snapshot = row(pool).await.expect("sticky retained");
    assert_eq!(
        snapshot["last_message_id"], OLD,
        "unconfirmed replacement is not adopted"
    );
    let grant = store::claim_sticky_post(
        pool,
        GUILD_S,
        CHANNEL_S,
        "future-worker-probe",
        now_millis_for_test() + 120_000,
    )
    .await
    .expect("independent durable admission probe");
    assert!(
        grant.is_none(),
        "uncertainty survives the 60-second lease, not only an in-memory hold"
    );
}

async fn uncertain(first: FirstPost) {
    fixture(move |db| {
        Box::pin(async move {
            let discord = Discord::start(first, Cleanup::Deleted).await;
            let runtime = discord.runtime(db.pool().clone(), true);
            assert_ne!(
                activity(&runtime, 1).await,
                ActivityOutcome::Reposted,
                "possibly accepted is not confirmed"
            );
            let peer = db.independent_pool().await.expect("restart pool");
            drop(runtime);
            let restarted = discord.runtime(peer.clone(), true);
            let second = activity(&restarted, 2).await;
            assert_ne!(
                second,
                ActivityOutcome::Reposted,
                "fresh runtime must honor durable uncertainty"
            );
            assert_one_attempt(&discord);
            assert_eq!(
                discord.live_ids(),
                [OLD, ORPHAN],
                "old and ambiguous post remain, never a third sticky"
            );
            assert_fenced(&peer).await;
            drop(restarted);
            peer.close().await;
        })
    })
    .await;
}

#[tokio::test]
async fn accepted_then_response_lost_never_sends_a_fresh_nonce() {
    uncertain(FirstPost::LostResponse).await;
}

#[tokio::test]
async fn accepted_then_timeout_never_sends_a_fresh_nonce() {
    uncertain(FirstPost::Held).await;
}

macro_rules! unusable_id {
    ($name:ident, $body:literal) => {
        #[tokio::test]
        async fn $name() {
            uncertain(FirstPost::Body($body)).await;
        }
    };
}
unusable_id!(success_missing_id_retains_uncertainty, "{}");
unusable_id!(success_malformed_json_retains_uncertainty, "not JSON");
unusable_id!(success_null_id_retains_uncertainty, r#"{"id":null}"#);
unusable_id!(success_empty_id_retains_uncertainty, r#"{"id":""}"#);
unusable_id!(success_zero_id_retains_uncertainty, r#"{"id":"0"}"#);
unusable_id!(
    success_non_snowflake_id_retains_uncertainty,
    r#"{"id":"abc"}"#
);
unusable_id!(
    success_overflow_id_retains_uncertainty,
    r#"{"id":"18446744073709551616"}"#
);

async fn fail_first_record(pool: &PgPool) {
    sqlx::raw_sql(
        "CREATE FUNCTION refuse_first_sticky_record() RETURNS trigger AS $$
         BEGIN
           IF NEW.last_message_id = '9000000000000000100' THEN
             RAISE EXCEPTION 'synthetic record refusal';
           END IF;
           RETURN NEW;
         END; $$ LANGUAGE plpgsql;
         CREATE TRIGGER refuse_first_sticky_record BEFORE UPDATE ON sticky_messages
         FOR EACH ROW EXECUTE FUNCTION refuse_first_sticky_record();",
    )
    .execute(pool)
    .await
    .expect("install completion-only fault, claims and releases remain live");
}

async fn failed_record(cleanup: Cleanup, can_retry: bool) {
    fixture(move |db| {
        Box::pin(async move {
            fail_first_record(db.pool()).await;
            let discord = Discord::start(FirstPost::Confirm, cleanup).await;
            let runtime = discord.runtime(db.pool().clone(), true);
            assert_ne!(activity(&runtime, 10).await, ActivityOutcome::Reposted);
            assert_eq!(row(db.pool()).await.expect("row")["last_message_id"], OLD);
            assert_eq!(
                discord.state.lock().expect("state").deletes,
                [ORPHAN],
                "cleanup targets the orphan, not previous sticky"
            );
            let peer = db.independent_pool().await.expect("restart pool");
            drop(runtime);
            let restarted = discord.runtime(peer.clone(), true);
            let outcome = activity(&restarted, 11).await;
            if can_retry {
                assert_eq!(
                    outcome,
                    ActivityOutcome::Reposted,
                    "confirmed orphan absence permits retry"
                );
                let posts = discord.posts();
                assert_eq!(posts.len(), 2);
                assert_ne!(
                    posts[0]["nonce"], posts[1]["nonce"],
                    "safe retry is a new attempt"
                );
                assert_eq!(
                    row(&peer).await.expect("row")["last_message_id"],
                    "9000000000000000101"
                );
                assert_eq!(discord.live_ids(), ["9000000000000000101"]);
            } else {
                assert_ne!(
                    outcome,
                    ActivityOutcome::Reposted,
                    "failed cleanup is still uncertain"
                );
                assert_one_attempt(&discord);
                assert_eq!(discord.live_ids(), [OLD, ORPHAN]);
                assert_fenced(&peer).await;
            }
            drop(restarted);
            peer.close().await;
        })
    })
    .await;
}

#[tokio::test]
async fn record_refused_and_cleanup_forbidden_retains_uncertainty() {
    failed_record(Cleanup::Forbidden, false).await;
}

#[tokio::test]
async fn record_refused_and_cleanup_transport_lost_retains_uncertainty() {
    failed_record(Cleanup::LostResponse, false).await;
}

#[tokio::test]
async fn record_refused_but_cleanup_confirmed_allows_retry() {
    failed_record(Cleanup::Deleted, true).await;
}

#[tokio::test]
async fn record_refused_but_orphan_proven_absent_allows_retry() {
    failed_record(Cleanup::AlreadyAbsent, true).await;
}

#[tokio::test]
async fn crash_after_accept_and_expired_lease_cannot_admit_another_worker() {
    fixture(|db| {
        Box::pin(async move {
            let discord = Discord::start(FirstPost::Held, Cleanup::Deleted).await;
            let runtime = discord.runtime(db.pool().clone(), true);
            let worker = tokio::spawn(async move { activity(&runtime, 20).await });
            discord.entered().await;
            worker.abort();
            assert!(worker
                .await
                .expect_err("worker cancelled at accepted POST")
                .is_cancelled());
            let peer = db
                .independent_pool()
                .await
                .expect("independent restarted worker");
            // Age the ordinary claim, not any new delivery uncertainty state.
            sqlx::query(
                "UPDATE sticky_messages SET claimed_at = clock_timestamp() - interval '2 minutes'",
            )
            .execute(&peer)
            .await
            .expect("expire ordinary lease in disposable fixture");
            let restarted = discord.runtime(peer.clone(), true);
            assert_ne!(activity(&restarted, 21).await, ActivityOutcome::Reposted);
            assert_one_attempt(&discord);
            assert_fenced(&peer).await;
            drop(restarted);
            peer.close().await;
        })
    })
    .await;
}

#[tokio::test]
async fn obsolete_completion_after_admin_update_never_overwrites_new_body() {
    fixture(|db| {
        Box::pin(async move {
            let discord = Discord::start(FirstPost::Held, Cleanup::Deleted).await;
            // Use the ordinary executor budget so the explicit release controls the race.
            let runtime = CommandRuntime::new(
                db.pool().clone(),
                executor_at(discord.origin.clone()),
                super::router_with_commands(gates(true, true)),
                GUILD,
                true,
            );
            let worker = {
                let runtime = runtime.clone();
                tokio::spawn(async move { activity(&runtime, 30).await })
            };
            discord.entered().await;
            runtime
                .on_interaction(&slash(
                    "sticky",
                    Some(CHANNEL),
                    vec![
                        option(
                            "body",
                            CommandOptionValue::String("administrator replacement".into()),
                        ),
                        option("debounce", CommandOptionValue::Integer(5)),
                    ],
                ))
                .await;
            discord.finish.notify_one();
            assert_ne!(
                worker.await.expect("obsolete completion"),
                ActivityOutcome::Reposted
            );
            let persisted = row(db.pool()).await.expect("admin row");
            assert_eq!(persisted["body"], "administrator replacement");
            assert_eq!(persisted["last_message_id"], OLD);
            assert_eq!(discord.state.lock().expect("state").deletes, [ORPHAN]);
            assert_eq!(discord.live_ids(), [OLD]);
        })
    })
    .await;
}

#[tokio::test]
async fn obsolete_completion_with_failed_cleanup_after_admin_remove_cannot_resurrect() {
    fixture(|db| {
        Box::pin(async move {
            let discord = Discord::start(FirstPost::Held, Cleanup::Forbidden).await;
            let runtime = CommandRuntime::new(
                db.pool().clone(),
                executor_at(discord.origin.clone()),
                super::router_with_commands(gates(true, true)),
                GUILD,
                true,
            );
            let worker = {
                let runtime = runtime.clone();
                tokio::spawn(async move { activity(&runtime, 40).await })
            };
            discord.entered().await;
            assert!(matches!(
                store::delete_sticky(db.pool(), GUILD_S, CHANNEL_S)
                    .await
                    .expect("admin remove"),
                RemoveOutcome::Removed { .. }
            ));
            discord.finish.notify_one();
            assert_ne!(
                worker.await.expect("obsolete completion"),
                ActivityOutcome::Reposted
            );
            let peer = db.independent_pool().await.expect("restart after remove");
            drop(runtime);
            let restarted = discord.runtime(peer.clone(), true);
            assert_ne!(activity(&restarted, 41).await, ActivityOutcome::Reposted);
            assert!(row(&peer).await.is_none(), "removed sticky stays absent");
            assert_one_attempt(&discord);
            assert_eq!(
                discord.live_ids(),
                [OLD, ORPHAN],
                "unconfirmed cleanup is not modeled as successful"
            );
            drop(restarted);
            peer.close().await;
        })
    })
    .await;
}

#[derive(Debug)]
struct RefusedAdmission {
    key: TokenKey,
    refusal: Mutex<Option<AdmissionError>>,
    attempts: AtomicUsize,
}

struct CompletedAdmission;

impl SendCompletion for CompletedAdmission {
    fn complete(
        self: Box<Self>,
        _cooldown: Option<SendCooldown>,
    ) -> AdmissionFuture<'static, Result<(), AdmissionError>> {
        Box::pin(async { Ok(()) })
    }
}

impl SendAdmission for RefusedAdmission {
    fn token_key(&self) -> &TokenKey {
        &self.key
    }

    fn admit(&self) -> AdmissionFuture<'_, Result<AdmissionPermit, AdmissionError>> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        let refusal = *self.refusal.lock().expect("admission state");
        Box::pin(async move {
            match refusal {
                Some(error) => Err(error),
                None => Ok(AdmissionPermit::new(Box::new(CompletedAdmission))),
            }
        })
    }
}

async fn admission_refusal_allows_retry(refusal: AdmissionError) {
    fixture(move |db| {
        Box::pin(async move {
            let discord = Discord::start(FirstPost::Confirm, Cleanup::Deleted).await;
            // Inject only the durable admission boundary. The HTTP executor,
            // error conversion, sticky handler, fence and store remain real.
            let admission = Arc::new(RefusedAdmission {
                key: TokenKey::for_bot_token("test-token").expect("synthetic token key"),
                refusal: Mutex::new(Some(refusal)),
                attempts: AtomicUsize::new(0),
            });
            let executor = ActionExecutor::with_admission(
                "test-token".into(),
                Some(discord.origin.clone()),
                admission.clone(),
            )
            .expect("governed loopback executor");
            let runtime = CommandRuntime::new(
                db.pool().clone(),
                executor,
                super::router_with_commands(gates(true, true)),
                GUILD,
                true,
            );
            assert_ne!(activity(&runtime, 90).await, ActivityOutcome::Reposted);
            assert_eq!(
                admission.attempts.load(Ordering::SeqCst),
                1,
                "sticky must reach the real executor's admission boundary"
            );
            assert!(
                discord.posts().is_empty(),
                "{refusal:?} is refused before any HTTP POST"
            );
            assert!(discord.state.lock().expect("state").deletes.is_empty());
            assert_eq!(discord.live_ids(), [OLD]);
            let persisted = row(db.pool()).await.expect("refused sticky retained");
            assert_eq!(persisted["last_message_id"], OLD);
            assert_eq!(
                persisted["delivery_uncertain"], false,
                "{refusal:?} proves no send and must release the delivery fence"
            );
            assert_eq!(
                persisted["claim_token"],
                serde_json::Value::Null,
                "{refusal:?} must release the sticky claim"
            );
            assert_eq!(persisted["claimed_at"], serde_json::Value::Null);

            *admission.refusal.lock().expect("admission state") = None;
            assert_eq!(
                activity(&runtime, 91).await,
                ActivityOutcome::Reposted,
                "later admitted activity must not remain wedged by {refusal:?}"
            );
            assert_one_attempt(&discord);
            assert_eq!(discord.live_ids(), [ORPHAN]);
            assert_eq!(discord.state.lock().expect("state").deletes, [OLD]);
            assert_eq!(
                row(db.pool()).await.expect("posted sticky")["last_message_id"],
                ORPHAN
            );
            assert_ne!(activity(&runtime, 92).await, ActivityOutcome::Reposted);
            assert_one_attempt(&discord);
        })
    })
    .await;
}

#[tokio::test]
async fn admission_storage_refusal_releases_fence_without_posting() {
    admission_refusal_allows_retry(AdmissionError::Storage).await;
}

#[tokio::test]
async fn admission_configuration_refusal_releases_fence_without_posting() {
    admission_refusal_allows_retry(AdmissionError::Configuration).await;
}

#[tokio::test]
async fn admission_stale_claim_refusal_releases_fence_without_posting() {
    admission_refusal_allows_retry(AdmissionError::StaleClaim).await;
}

#[tokio::test]
async fn explicit_no_send_rejection_permits_a_safe_new_attempt() {
    fixture(|db| {
        Box::pin(async move {
            let discord = Discord::start(FirstPost::Rejected, Cleanup::Deleted).await;
            let runtime = discord.runtime(db.pool().clone(), true);
            assert_ne!(activity(&runtime, 50).await, ActivityOutcome::Reposted);
            assert_eq!(discord.live_ids(), [OLD], "403 did not create a message");
            assert_eq!(activity(&runtime, 51).await, ActivityOutcome::Reposted);
            let posts = discord.posts();
            assert_eq!(posts.len(), 2);
            assert_ne!(posts[0]["nonce"], posts[1]["nonce"]);
            assert_eq!(discord.live_ids(), ["9000000000000000101"]);
            assert_eq!(
                row(db.pool()).await.expect("row")["last_message_id"],
                "9000000000000000101"
            );
        })
    })
    .await;
}

#[tokio::test]
async fn confirmed_replacement_retires_previous_and_debounces_followup() {
    fixture(|db| {
        Box::pin(async move {
            let discord = Discord::start(FirstPost::Confirm, Cleanup::Deleted).await;
            let runtime = discord.runtime(db.pool().clone(), true);
            assert_eq!(activity(&runtime, 60).await, ActivityOutcome::Reposted);
            assert_ne!(activity(&runtime, 61).await, ActivityOutcome::Reposted);
            assert_one_attempt(&discord);
            assert_eq!(discord.live_ids(), [ORPHAN]);
            assert_eq!(
                row(db.pool()).await.expect("row")["last_message_id"],
                ORPHAN
            );
        })
    })
    .await;
}

#[tokio::test]
async fn disabled_automation_and_ineligible_activity_touch_no_delivery_state() {
    fixture(|db| {
        Box::pin(async move {
            let discord = Discord::start(FirstPost::Confirm, Cleanup::Deleted).await;
            let before = row(db.pool()).await;
            let disabled = discord.runtime(db.pool().clone(), false);
            assert_ne!(activity(&disabled, 70).await, ActivityOutcome::Reposted);
            let enabled = discord.runtime(db.pool().clone(), true);
            for msg in [
                message(71, CHANNEL, true, Some(GUILD)),
                message(72, CHANNEL, false, Some(9999)),
                message(73, CHANNEL, false, None),
            ] {
                assert_ne!(enabled.on_message(&msg).await, ActivityOutcome::Reposted);
            }
            assert!(discord.posts().is_empty());
            assert_eq!(row(db.pool()).await, before);
        })
    })
    .await;
}

#[tokio::test]
async fn uncertainty_emits_safe_structured_recovery_diagnostic_not_success() {
    fixture(|db| Box::pin(async move {
        let capture = crate::tracing_capture::global();
        let start = capture.text().len();
        let discord = Discord::start(FirstPost::LostResponse, Cleanup::Deleted).await;
        let runtime = discord.runtime(db.pool().clone(), true);
        assert_ne!(activity(&runtime, 80).await, ActivityOutcome::Reposted);
        let text = capture.text();
        let logs = &text[start..];
        let diagnostic = logs.lines().find(|line|
            line.contains("code=\"sticky_delivery_uncertain\"")
            && line.contains("next_action=\"inspect_channel_before_sticky_reset\"")
            && line.contains("retryable=false"));
        assert!(diagnostic.is_some(), "uncertainty needs a bounded code and inspection-first recovery action; logs: {logs}");
        for forbidden in [BODY, "test-token", "sticky-test-token", "member activity"] {
            assert!(!logs.contains(forbidden), "no token, sticky body or member content in logs");
        }
        let successes: i64 = sqlx::query_scalar("SELECT count(*) FROM automation_audit_log WHERE action = 'sticky.run' AND outcome = 'ok'")
            .fetch_one(db.pool()).await.expect("success audit count");
        assert_eq!(successes, 0, "unconfirmed replacement cannot be audited as success");
    })).await;
}
