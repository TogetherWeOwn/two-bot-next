//! Audit runtime acceptance (TOG-12240): env gating, the `audit_retry` sweep
//! against the migrated agent-testdb/CI Postgres fixture, an in-process
//! mirror double plus one loopback-REST pass through the real executor, job
//! metrics and log hygiene. No Discord endpoint, guild or token is contacted.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::json;
use two_bot_core::audit::{format_audit_event, AuditKind};
use two_bot_core::audit_mirror::{MirrorChannel, MirrorError, MirrorMessage, MirrorOverwrite};
use two_bot_core::audit_store::{DeliveryState, QuarantineReason};
use two_bot_testsupport::TestDatabase;

use super::*;

use crate::discord_test_common::{MockRest, ScriptedResponse};

const GUILD: &str = "18446744073709551615";
const AUDIT_CH: &str = "1111";
const VOICE_CH: &str = "2222";
const MOD_CH: &str = "3333";
const BOT: &str = "9999";
const OPERATOR: &str = "18446744073709551611";

// ------------------------------------------------------------- env gating --

fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

#[test]
fn unconfigured_audit_parks_disabled() {
    assert_eq!(resolve_channels(&HashMap::new()), Err("disabled"));
    let blank = vars(&[
        ("DISCORD_AUDIT_LOG_CHANNEL_ID", ""),
        ("DISCORD_VOICE_LOG_CHANNEL_ID", "   "),
    ]);
    assert_eq!(resolve_channels(&blank), Err("disabled"));
}

#[test]
fn malformed_destination_parks_invalid_config() {
    for bad in [
        "0",
        "-5",
        "+5",
        "012",
        " 5",
        "5 ",
        "abc",
        "18446744073709551616",
    ] {
        let input = vars(&[("DISCORD_MODERATION_LOG_CHANNEL_ID", bad)]);
        assert_eq!(resolve_channels(&input), Err("invalid_config"), "{bad:?}");
    }
    let mixed = vars(&[
        ("DISCORD_AUDIT_LOG_CHANNEL_ID", AUDIT_CH),
        ("DISCORD_VOICE_LOG_CHANNEL_ID", "voice"),
    ]);
    assert_eq!(resolve_channels(&mixed), Err("invalid_config"));
}

#[test]
fn any_destination_enables_the_runtime() {
    let voice_only = vars(&[("DISCORD_VOICE_LOG_CHANNEL_ID", VOICE_CH)]);
    assert_eq!(
        resolve_channels(&voice_only),
        Ok(AuditChannelIds {
            audit: None,
            voice: Some(VOICE_CH.to_owned()),
            moderation: None,
        })
    );
    assert_eq!(resolve_channels(&all_vars()), Ok(channels()));
}

fn all_vars() -> HashMap<String, String> {
    vars(&[
        ("DISCORD_AUDIT_LOG_CHANNEL_ID", AUDIT_CH),
        ("DISCORD_VOICE_LOG_CHANNEL_ID", VOICE_CH),
        ("DISCORD_MODERATION_LOG_CHANNEL_ID", MOD_CH),
    ])
}

fn channels() -> AuditChannelIds {
    AuditChannelIds {
        audit: Some(AUDIT_CH.to_owned()),
        voice: Some(VOICE_CH.to_owned()),
        moderation: Some(MOD_CH.to_owned()),
    }
}

// ---------------------------------------------------------- mirror double --

type PostResult = Result<String, MirrorError>;
type DocResult = Result<MirrorChannel, MirrorError>;
type PageResult = Result<Vec<MirrorMessage>, MirrorError>;

#[derive(Debug, Clone)]
struct Post {
    channel_id: String,
    content: String,
}

/// In-memory `AuditMirror`: scripted response queues (emptied queues fall
/// back to a private in-guild channel, an empty history page and an accepted
/// post), recorded posts, a call counter, and an optional operator pool that
/// engages the persistent halt during the next channel read — i.e. after the
/// row was claimed and before its POST.
#[derive(Clone, Default)]
struct ScriptMirror {
    posts: Arc<Mutex<Vec<Post>>>,
    post_script: Arc<Mutex<VecDeque<PostResult>>>,
    documents: Arc<Mutex<VecDeque<DocResult>>>,
    history: Arc<Mutex<VecDeque<PageResult>>>,
    calls: Arc<AtomicUsize>,
    halt_on_document: Arc<Mutex<Option<PgPool>>>,
}

impl ScriptMirror {
    fn posts(&self) -> Vec<Post> {
        self.posts.lock().unwrap().clone()
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn script_post(&self, result: PostResult) {
        self.post_script.lock().unwrap().push_back(result);
    }

    fn script_document(&self, result: DocResult) {
        self.documents.lock().unwrap().push_back(result);
    }

    fn script_history(&self, page: Vec<MirrorMessage>) {
        self.history.lock().unwrap().push_back(Ok(page));
    }
}

fn private_document() -> MirrorChannel {
    MirrorChannel {
        guild_id: GUILD.to_owned(),
        everyone: Some(MirrorOverwrite {
            allow: "0".to_owned(),
            // VIEW_CHANNEL denied to @everyone: the private-mirror gate.
            deny: "1024".to_owned(),
        }),
    }
}

impl AuditMirror for ScriptMirror {
    async fn post_mirror_checked<Fut, E>(
        &self,
        channel_id: &str,
        content: &str,
        _nonce: &str,
        authorize: Fut,
    ) -> Result<Result<String, MirrorError>, E>
    where
        Fut: std::future::Future<Output = Result<(), E>> + Send,
        E: Send,
    {
        self.calls.fetch_add(1, Ordering::SeqCst);
        authorize.await?;
        self.posts.lock().unwrap().push(Post {
            channel_id: channel_id.to_owned(),
            content: content.to_owned(),
        });
        Ok(self
            .post_script
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Ok("900000".to_owned())))
    }

    async fn channel_document(&self, _channel_id: &str) -> Result<MirrorChannel, MirrorError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let operator = self.halt_on_document.lock().unwrap().take();
        if let Some(pool) = operator {
            assert!(AuditStore::new(&pool).engage_halt(OPERATOR).await.unwrap());
        }
        self.documents
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Ok(private_document()))
    }

    async fn channel_history(
        &self,
        _channel_id: &str,
        _before: Option<&str>,
        _limit: u8,
    ) -> Result<Vec<MirrorMessage>, MirrorError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.history
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Ok(vec![]))
    }
}

// ----------------------------------------------------------------- shared --

fn event(entry: &str) -> AuditEvent {
    let mut e = AuditEvent::new(
        entry.to_owned(),
        AuditKind::MemberUpdate,
        GUILD.to_owned(),
        "2026-09-30T00:01:02.003Z".to_owned(),
    );
    e.actor_id = Some("18446744073709551614".to_owned());
    e.target_id = Some("18446744073709551613".to_owned());
    e.metadata_json = r#"{"nicknameChanged":true}"#.to_owned();
    e
}

fn marked(id: &str, event: &AuditEvent) -> MirrorMessage {
    MirrorMessage {
        id: id.to_owned(),
        author_id: BOT.to_owned(),
        content: format_audit_event(event),
    }
}

async fn database(test: &str) -> Option<TestDatabase> {
    let Ok(url) = std::env::var("TWO_TEST_DATABASE_URL") else {
        assert!(
            std::env::var("GITHUB_ACTIONS").is_err(),
            "CI must supply the guarded test database"
        );
        eprintln!("SKIP {test}: TWO_TEST_DATABASE_URL is not set");
        return None;
    };
    Some(
        TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("create migrated agent-testdb fixture"),
    )
}

fn runtime(pool: &PgPool, mirror: &ScriptMirror) -> Arc<AuditRuntime<ScriptMirror>> {
    let (pool, mirror) = (pool.clone(), mirror.clone());
    Arc::new(AuditRuntime::new(
        channels(),
        GUILD.to_owned(),
        Box::new(move || {
            let parts = Parts {
                pool: pool.clone(),
                mirror: mirror.clone(),
                bot_user_id: BOT.to_owned(),
            };
            Box::pin(async move { Ok(parts) })
        }),
    ))
}

async fn record(runtime: &AuditRuntime<ScriptMirror>, entry: &str) {
    assert_eq!(
        runtime.record(&event(entry)).await.unwrap(),
        RecordOutcome::Queued {
            mirror_channel_id: AUDIT_CH.to_owned()
        }
    );
}

async fn drained<M: AuditMirror>(runtime: &AuditRuntime<M>) -> Vec<(String, DeliverOutcome)> {
    match runtime.sweep().await.unwrap() {
        Sweep::Drained(report) => {
            assert!(
                report.failed.is_empty(),
                "store failures: {}",
                report.failed.len()
            );
            report.deliveries
        }
        Sweep::Halted => panic!("sweep unexpectedly halted"),
    }
}

fn delivered(entry: &str, message_id: &str) -> (String, DeliverOutcome) {
    (
        entry.to_owned(),
        DeliverOutcome::Delivered {
            message_id: message_id.to_owned(),
        },
    )
}

// --------------------------------------------------------------- sweeping --

#[tokio::test]
async fn pending_row_is_delivered_exactly_once() {
    let Some(db) = database("pending_row_is_delivered_exactly_once").await else {
        return;
    };
    let mirror = ScriptMirror::default();
    let runtime = runtime(db.pool(), &mirror);
    record(&runtime, "once-1").await;

    assert_eq!(drained(&runtime).await, [delivered("once-1", "900000")]);
    let posts = mirror.posts();
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0].channel_id, AUDIT_CH);
    assert_eq!(posts[0].content, format_audit_event(&event("once-1")));

    assert!(
        drained(&runtime).await.is_empty(),
        "a delivered row is never resent"
    );
    assert_eq!(mirror.posts().len(), 1);
    let row = AuditStore::new(db.pool())
        .get("once-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.state, DeliveryState::Delivered);
    assert_eq!(row.mirror_message_id.as_deref(), Some("900000"));
    db.close().await.unwrap();
}

#[tokio::test]
async fn halt_skips_sends_and_releases_held_claims() {
    let Some(db) = database("halt_skips_sends_and_releases_held_claims").await else {
        return;
    };
    let mirror = ScriptMirror::default();
    let runtime = runtime(db.pool(), &mirror);
    let operator = db.independent_pool().await.unwrap();
    record(&runtime, "halt-1").await;

    // The operator engages the halt after the claim, before the POST.
    *mirror.halt_on_document.lock().unwrap() = Some(operator.clone());
    assert_eq!(
        drained(&runtime).await,
        [("halt-1".to_owned(), DeliverOutcome::Held)]
    );
    assert!(mirror.posts().is_empty());
    let store = AuditStore::new(db.pool());
    let row = store.get("halt-1").await.unwrap().unwrap();
    assert_eq!(row.state, DeliveryState::Pending, "held claim released");
    assert_eq!(row.attempts, 0, "released unattempted");

    // While the halt stays engaged the sweep claims nothing at all.
    let calls = mirror.calls();
    assert!(matches!(runtime.sweep().await.unwrap(), Sweep::Halted));
    assert_eq!(mirror.calls(), calls, "no mirror traffic while halted");

    assert!(AuditStore::new(&operator).disengage_halt().await.unwrap());
    assert_eq!(drained(&runtime).await, [delivered("halt-1", "900000")]);
    assert_eq!(mirror.posts().len(), 1);
    operator.close().await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn ambiguous_post_is_reconciled_without_a_resend() {
    let Some(db) = database("ambiguous_post_is_reconciled_without_a_resend").await else {
        return;
    };
    let mirror = ScriptMirror::default();
    let runtime = runtime(db.pool(), &mirror);
    record(&runtime, "amb-1").await;

    mirror.script_history(vec![MirrorMessage {
        id: "500".to_owned(),
        author_id: BOT.to_owned(),
        content: "older traffic".to_owned(),
    }]);
    mirror.script_post(Err(MirrorError::Uncertain("socket closed".to_owned())));
    assert_eq!(
        drained(&runtime).await,
        [("amb-1".to_owned(), DeliverOutcome::Ambiguous)]
    );
    assert_eq!(mirror.posts().len(), 1);
    let row = AuditStore::new(db.pool())
        .get("amb-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.search_before.as_deref(), Some("501"));

    // The next sweep owns a reconcile-only claim: it finds the accepted
    // mirror after the boundary and adopts it instead of posting again.
    mirror.script_history(vec![marked("777", &event("amb-1"))]);
    assert_eq!(
        drained(&runtime).await,
        [(
            "amb-1".to_owned(),
            DeliverOutcome::Reconciled {
                message_id: "777".to_owned()
            }
        )]
    );
    assert_eq!(mirror.posts().len(), 1, "no blind resend");
    let row = AuditStore::new(db.pool())
        .get("amb-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.state, DeliveryState::Delivered);
    assert_eq!(row.mirror_message_id.as_deref(), Some("777"));
    db.close().await.unwrap();
}

#[tokio::test]
async fn one_sweep_claims_at_most_twenty_five_rows() {
    let Some(db) = database("one_sweep_claims_at_most_twenty_five_rows").await else {
        return;
    };
    let mirror = ScriptMirror::default();
    let runtime = runtime(db.pool(), &mirror);
    for n in 0..30 {
        record(&runtime, &format!("cap-{n:02}")).await;
    }

    let first = drained(&runtime).await;
    assert_eq!(first.len(), 25);
    assert_eq!(mirror.posts().len(), 25);
    let store = AuditStore::new(db.pool());
    assert_eq!(store.pending_ids().await.unwrap().len(), 5);

    assert_eq!(drained(&runtime).await.len(), 5);
    assert_eq!(mirror.posts().len(), 30);
    assert!(store.pending_ids().await.unwrap().is_empty());
    db.close().await.unwrap();
}

#[tokio::test]
async fn sweep_delivers_through_the_rest_executor() {
    let Some(db) = database("sweep_delivers_through_the_rest_executor").await else {
        return;
    };
    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(200, json!({"id": BOT, "bot": true})),
            ScriptedResponse::json(
                200,
                json!({
                    "id": AUDIT_CH,
                    "guild_id": GUILD,
                    "permission_overwrites": [
                        {"id": GUILD, "type": 0, "allow": "0", "deny": "1024"}
                    ]
                }),
            ),
            ScriptedResponse::json(200, json!([])),
            ScriptedResponse::json(200, json!({"id": "640"})),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    crate::gateway::ensure_crypto_provider();
    let rest =
        ActionExecutor::with_proxy("synthetic-audit-test-token".to_owned(), Some(mock.origin()))
            .unwrap();
    let pool = db.pool().clone();
    let runtime: AuditRuntime<ActionExecutor> = AuditRuntime::new(
        channels(),
        GUILD.to_owned(),
        Box::new(move || Box::pin(rest_parts(pool.clone(), rest.clone()))),
    );
    assert_eq!(
        runtime.record(&event("rest-1")).await.unwrap(),
        RecordOutcome::Queued {
            mirror_channel_id: AUDIT_CH.to_owned()
        }
    );

    assert_eq!(drained(&runtime).await, [delivered("rest-1", "640")]);
    let requests = mock.requests();
    let routes: Vec<_> = requests
        .iter()
        .map(|r| (r.method.as_str(), r.path.split('?').next().unwrap()))
        .collect();
    assert_eq!(
        routes,
        [
            ("GET", "/api/v10/users/@me"),
            ("GET", "/api/v10/channels/1111"),
            ("GET", "/api/v10/channels/1111/messages"),
            ("POST", "/api/v10/channels/1111/messages"),
        ]
    );
    mock.shutdown().await;
    db.close().await.unwrap();
}

// -------------------------------------------------------- metrics and logs --

/// The current value of one `/metrics` sample line.
fn sample(text: &str, series: &str) -> Option<u64> {
    text.lines()
        .find_map(|line| line.strip_prefix(series)?.strip_prefix(' ')?.parse().ok())
}

async fn supervise_once<M: AuditMirror + 'static>(
    runtime: Arc<AuditRuntime<M>>,
) -> jobs::JobStatus {
    let status = jobs::statuses(&NAMES, false);
    let (stop, rx) = watch::channel(false);
    let job = retry_job(runtime, rx.clone(), Duration::ZERO);
    let task = tokio::spawn(jobs::supervise(vec![job], status.clone(), rx));
    let finished = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let current = status.read().await[JOB].clone();
            if current.last_success.is_some() || current.consecutive_failures > 0 {
                return current;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("audit_retry ran once");
    stop.send(true).unwrap();
    task.await.unwrap();
    finished
}

#[tokio::test]
async fn sweep_outcome_reaches_job_metrics() {
    assert!(two_bot_core::metrics::JOBS.contains(&JOB));
    let Some(db) = database("sweep_outcome_reaches_job_metrics").await else {
        return;
    };
    let mirror = ScriptMirror::default();
    let runtime = runtime(db.pool(), &mirror);
    record(&runtime, "metric-1").await;

    let status = supervise_once(runtime).await;
    assert!(status.last_success.is_some());
    assert_eq!(mirror.posts().len(), 1);
    let text = two_bot_core::metrics::global().render(None);
    let series = r#"two_bot_job_runs_total{job="audit_retry",outcome="success"}"#;
    assert!(
        sample(&text, series).is_some_and(|runs| runs >= 1),
        "{series}"
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn unreachable_dependencies_fail_the_job_and_retry_next_sweep() {
    let connects = Arc::new(AtomicUsize::new(0));
    let counter = connects.clone();
    let runtime: Arc<AuditRuntime<ScriptMirror>> = Arc::new(AuditRuntime::new(
        channels(),
        GUILD.to_owned(),
        Box::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Err(ErrorClass::Database) })
        }),
    ));

    let status = supervise_once(runtime.clone()).await;
    assert_eq!(status.last_error_class, Some(ErrorClass::Database));
    let text = two_bot_core::metrics::global().render(None);
    let series = r#"two_bot_job_runs_total{job="audit_retry",outcome="failure"}"#;
    assert!(
        sample(&text, series).is_some_and(|runs| runs >= 1),
        "{series}"
    );

    assert!(matches!(runtime.sweep().await, Err(ErrorClass::Database)));
    assert_eq!(
        connects.load(Ordering::SeqCst),
        2,
        "a failed connect is not cached"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn sweep_logs_carry_ids_and_counts_only() {
    let Some(db) = database("sweep_logs_carry_ids_and_counts_only").await else {
        return;
    };
    let mirror = ScriptMirror::default();
    let runtime = runtime(db.pool(), &mirror);
    for entry in ["log-1", "log-2", "log-3"] {
        record(&runtime, entry).await;
    }
    // Claim order is creation order: log-1 posts, log-2's POST is ambiguous,
    // log-3's destination read is refused.
    mirror.script_document(Ok(private_document()));
    mirror.script_document(Ok(private_document()));
    mirror.script_document(Err(MirrorError::Rejected(
        "missing access 50001".to_owned(),
    )));
    mirror.script_post(Ok("900001".to_owned()));
    mirror.script_post(Err(MirrorError::Uncertain(
        "transport detail 7f3a".to_owned(),
    )));

    // The fmt-writer capture recorded nothing (never green since #257):
    // capture with the repo tracing helper instead.
    let capture = crate::tracing_capture::Capture::default();
    let deliveries = {
        let _guard = tracing::subscriber::set_default(capture.clone());
        drained(&runtime).await
    };
    assert_eq!(
        deliveries,
        [
            delivered("log-1", "900001"),
            ("log-2".to_owned(), DeliverOutcome::Ambiguous),
            (
                "log-3".to_owned(),
                DeliverOutcome::Quarantined(QuarantineReason::PermissionRevoked)
            ),
        ]
    );

    let logs = capture.text();
    for expected in [
        "audit_retry_swept",
        "rows=3",
        "delivered=1",
        "ambiguous=1",
        "quarantined=1",
        "failed=0",
        "audit_entry_quarantined",
        "entry_id=log-3",
        "reason=PermissionRevoked",
    ] {
        assert!(logs.contains(expected), "missing {expected:?} in logs");
    }
    let content = format_audit_event(&event("log-1"));
    for leaked in [
        "nicknameChanged",
        "18446744073709551614",
        "18446744073709551613",
        "transport detail",
        "missing access",
        content.as_str(),
    ] {
        assert!(!logs.contains(leaked), "logs leaked {leaked:?}");
    }
    db.close().await.unwrap();
}
