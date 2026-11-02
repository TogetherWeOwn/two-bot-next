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
use two_bot_core::metrics::Metrics;
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

// ------------------------------------------------------- live destinations --

use two_bot_core::settings::{live_channel, SettingRow, SettingsSnapshot};

/// A runtime whose connector is never called: destination refresh is pure.
fn unconnected_runtime(deployment: HashMap<String, String>) -> AuditRuntime<ScriptMirror> {
    AuditRuntime::new(
        deployment,
        GUILD.to_owned(),
        Box::new(move || {
            Box::pin(async { Err::<Parts<ScriptMirror>, ErrorClass>(ErrorClass::Database) })
        }),
    )
    .unwrap()
}

#[test]
fn stored_destination_rows_move_the_live_mirror_without_restart() {
    let (mut writer, live) = live_channel();
    let runtime = unconnected_runtime(all_vars());
    assert_eq!(runtime.channels_for_test(), channels());

    // A stored audit row wins over the boot deployment value on the next
    // refresh; the other two destinations are untouched.
    writer.publish(&SettingsSnapshot {
        revision: 1,
        rows: vec![SettingRow {
            guild_id: GUILD.to_owned(),
            key: "DISCORD_AUDIT_LOG_CHANNEL_ID".to_owned(),
            value: json!("4444"),
            version: 1,
        }],
    });
    runtime.refresh_channels_with(Some(&live));
    let moved = runtime.channels_for_test();
    assert_eq!(moved.audit.as_deref(), Some("4444"));
    assert_eq!(moved.voice.as_deref(), Some(VOICE_CH));
    assert_eq!(moved.moderation.as_deref(), Some(MOD_CH));

    // Deleting the row hands the key back to the boot deployment value.
    writer.publish(&SettingsSnapshot {
        revision: 2,
        rows: vec![],
    });
    runtime.refresh_channels_with(Some(&live));
    assert_eq!(runtime.channels_for_test(), channels());
}

#[test]
fn malformed_stored_destination_keeps_the_last_good() {
    let (mut writer, live) = live_channel();
    let runtime = unconnected_runtime(all_vars());

    writer.publish(&SettingsSnapshot {
        revision: 1,
        rows: vec![SettingRow {
            guild_id: GUILD.to_owned(),
            key: "DISCORD_VOICE_LOG_CHANNEL_ID".to_owned(),
            value: json!("not-a-snowflake"),
            version: 1,
        }],
    });
    runtime.refresh_channels_with(Some(&live));
    assert_eq!(
        runtime.channels_for_test(),
        channels(),
        "a malformed stored row must not move or park a running mirror"
    );
}

#[test]
fn blank_stored_destinations_disable_the_live_mirror() {
    let (mut writer, live) = live_channel();
    let runtime = unconnected_runtime(all_vars());

    writer.publish(&SettingsSnapshot {
        revision: 1,
        rows: vec![
            SettingRow {
                guild_id: GUILD.to_owned(),
                key: "DISCORD_AUDIT_LOG_CHANNEL_ID".to_owned(),
                value: json!(""),
                version: 1,
            },
            SettingRow {
                guild_id: GUILD.to_owned(),
                key: "DISCORD_VOICE_LOG_CHANNEL_ID".to_owned(),
                value: json!(""),
                version: 1,
            },
            SettingRow {
                guild_id: GUILD.to_owned(),
                key: "DISCORD_MODERATION_LOG_CHANNEL_ID".to_owned(),
                value: json!(""),
                version: 1,
            },
        ],
    });
    runtime.refresh_channels_with(Some(&live));
    assert_eq!(
        runtime.channels_for_test(),
        AuditChannelIds::default(),
        "blanking all three destinations must park the live mirror"
    );
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
    Arc::new(
        AuditRuntime::new(
            all_vars(),
            GUILD.to_owned(),
            Box::new(move || {
                let parts = Parts {
                    pool: pool.clone(),
                    mirror: mirror.clone(),
                    bot_user_id: BOT.to_owned(),
                };
                Box::pin(async move { Ok(parts) })
            }),
        )
        .unwrap(),
    )
}

fn runtime_with_metrics(
    pool: &PgPool,
    mirror: &ScriptMirror,
    metrics: &'static Metrics,
) -> Arc<AuditRuntime<ScriptMirror>> {
    let (pool, mirror) = (pool.clone(), mirror.clone());
    Arc::new(
        AuditRuntime::with_metrics(
            all_vars(),
            GUILD.to_owned(),
            Box::new(move || {
                let parts = Parts {
                    pool: pool.clone(),
                    mirror: mirror.clone(),
                    bot_user_id: BOT.to_owned(),
                };
                Box::pin(async move { Ok(parts) })
            }),
            metrics,
        )
        .unwrap(),
    )
}

/// The exact `two_bot_audit_delivery_halt` series from an isolated registry.
/// Deleting the sweep's gauge write leaves the default 0, and inverting it
/// flips every expectation, so these asserts fail either way.
fn halt_gauge(metrics: &Metrics) -> String {
    metrics
        .render(None)
        .lines()
        .find(|line| line.starts_with("two_bot_audit_delivery_halt "))
        .expect("halt gauge in exposition")
        .to_owned()
}

async fn record(runtime: &AuditRuntime<ScriptMirror>, entry: &str) {
    assert_eq!(
        runtime.record(&event(entry)).await.unwrap(),
        RecordOutcome::Queued {
            mirror_channel_id: AUDIT_CH.to_owned()
        }
    );
}

async fn drained<M: AuditMirror + Clone>(
    runtime: &AuditRuntime<M>,
) -> Vec<(String, DeliverOutcome)> {
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

#[test]
fn halt_transitions_cover_engage_disengage_and_clear() {
    // M4.48: the same booleans the sweep feeds to `two_bot_audit_delivery_halt`
    // (1 engaged, 0 cleared) transition here. No database: the first clear
    // from a fresh process reports clear with no edge, engage reports halted,
    // a repeat reports the same state with no new edge, and disengage returns
    // to clear.
    let runtime = unconnected_runtime(all_vars());
    assert_eq!(runtime.halted_for_test(), None);
    assert!(!runtime.note_halt(false));
    assert_eq!(runtime.halted_for_test(), Some(false));
    assert!(runtime.note_halt(true));
    assert_eq!(runtime.halted_for_test(), Some(true));
    assert!(runtime.note_halt(true));
    assert_eq!(runtime.halted_for_test(), Some(true));
    assert!(!runtime.note_halt(false));
    assert_eq!(runtime.halted_for_test(), Some(false));
    assert!(!runtime.note_halt(false));
    assert_eq!(runtime.halted_for_test(), Some(false));
    assert!(runtime.note_halt(true));
    assert_eq!(runtime.halted_for_test(), Some(true));
}

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
    let metrics: &'static Metrics = Box::leak(Box::new(Metrics::default()));
    let runtime = runtime_with_metrics(db.pool(), &mirror, metrics);
    let operator = db.independent_pool().await.unwrap();
    record(&runtime, "halt-1").await;
    assert_eq!(runtime.halted_for_test(), None);

    // The operator engages the halt after the claim, before the POST.
    *mirror.halt_on_document.lock().unwrap() = Some(operator.clone());
    assert_eq!(
        drained(&runtime).await,
        [("halt-1".to_owned(), DeliverOutcome::Held)]
    );
    assert!(mirror.posts().is_empty());
    // The sweep started unhalted, so the halt memory stays clear even though
    // the service honored the mid-sweep halt per row. The sweep read an
    // unhalted store, so the gauge reports clear.
    assert_eq!(runtime.halted_for_test(), Some(false));
    assert_eq!(halt_gauge(metrics), "two_bot_audit_delivery_halt 0");
    let store = AuditStore::new(db.pool());
    let row = store.get("halt-1").await.unwrap().unwrap();
    assert_eq!(row.state, DeliveryState::Pending, "held claim released");
    assert_eq!(row.attempts, 0, "released unattempted");

    // While the halt stays engaged the sweep claims nothing at all.
    let calls = mirror.calls();
    assert!(matches!(runtime.sweep().await.unwrap(), Sweep::Halted));
    assert_eq!(mirror.calls(), calls, "no mirror traffic while halted");
    assert_eq!(runtime.halted_for_test(), Some(true));
    assert_eq!(halt_gauge(metrics), "two_bot_audit_delivery_halt 1");

    assert!(AuditStore::new(&operator).disengage_halt().await.unwrap());
    assert_eq!(drained(&runtime).await, [delivered("halt-1", "900000")]);
    assert_eq!(mirror.posts().len(), 1);
    assert_eq!(runtime.halted_for_test(), Some(false));
    assert_eq!(halt_gauge(metrics), "two_bot_audit_delivery_halt 0");
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
        all_vars(),
        GUILD.to_owned(),
        Box::new(move || Box::pin(rest_parts(pool.clone(), rest.clone()))),
    )
    .unwrap();
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

async fn supervise_once<M: AuditMirror + Clone + 'static>(
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
    task.await.unwrap().unwrap();
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
    let runtime: Arc<AuditRuntime<ScriptMirror>> = Arc::new(
        AuditRuntime::new(
            all_vars(),
            GUILD.to_owned(),
            Box::new(move || {
                counter.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { Err(ErrorClass::Database) })
            }),
        )
        .unwrap(),
    );

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

/// Process-wide sweep-log capture shared with every log-hygiene test in this
/// binary (see `tracing_capture::global`): one install per process, each
/// reader keeps only the text appended after its own start offset.
fn sweep_log_capture() -> &'static crate::tracing_capture::Capture {
    crate::tracing_capture::global()
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

    let capture = sweep_log_capture();
    let start = capture.text().len();
    let deliveries = drained(&runtime).await;
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

    let logs = capture.text().split_off(start);
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

// ------------------------------------------- refresh coherence acceptance --
// Protected acceptance suite: destination channels and the settings revision
// must stay coherent under refresh. Written against the existing test surface
// only (`refresh_channels_with`, `channels_for_test`, the live settings
// channel); the implementation handoff must make these pass without editing
// this section. No database, poller or Discord is involved.

fn acceptance_row(key: &str, value: &str) -> SettingRow {
    SettingRow {
        guild_id: GUILD.to_owned(),
        key: key.to_owned(),
        value: json!(value),
        version: 1,
    }
}

/// Deterministic RED on the baseline: the settings writer republishes to live
/// readers when the row set changes even at an unchanged revision
/// (`changed` covers row count), so a refresh that skips solely because the
/// revision matches its cache strands the previous destinations under the
/// current revision, and every later current-publication call also skips.
/// The outcome contract requires that call to repair the destinations to the
/// currently published snapshot.
#[test]
fn acceptance_current_publication_refresh_repairs_destinations_at_reused_revision() {
    let (mut writer, live) = live_channel();
    let runtime = unconnected_runtime(all_vars());

    writer.publish(&SettingsSnapshot {
        revision: 7,
        rows: vec![acceptance_row("DISCORD_AUDIT_LOG_CHANNEL_ID", "4444")],
    });
    runtime.refresh_channels_with(Some(&live));
    assert_eq!(runtime.channels_for_test().audit.as_deref(), Some("4444"));

    // Same revision, different published row set: this reaches live readers.
    writer.publish(&SettingsSnapshot {
        revision: 7,
        rows: vec![
            acceptance_row("DISCORD_AUDIT_LOG_CHANNEL_ID", "5555"),
            acceptance_row("DISCORD_VOICE_LOG_CHANNEL_ID", "6666"),
        ],
    });
    assert_eq!(live.revision(), 7);
    runtime.refresh_channels_with(Some(&live));
    let repaired = runtime.channels_for_test();
    assert_eq!(
        repaired.audit.as_deref(),
        Some("5555"),
        "a current-publication refresh must not skip needed repair when the \
         cached revision already matches"
    );
    assert_eq!(repaired.voice.as_deref(), Some("6666"));
    assert_eq!(repaired.moderation.as_deref(), Some(MOD_CH));

    // Rows deleted again at the same revision: destinations fall back to the
    // boot deployment values instead of stranding the last stored ones.
    writer.publish(&SettingsSnapshot {
        revision: 7,
        rows: vec![],
    });
    runtime.refresh_channels_with(Some(&live));
    assert_eq!(
        runtime.channels_for_test(),
        channels(),
        "deleting every stored row hands destinations back to deployment"
    );
}

/// Competing refreshes (record and sweep both reach refresh through the wired
/// path) must converge: after they quiesce, one more refresh against the
/// current publication leaves exactly that publication's destinations
/// installed. A late completion of an older snapshot's refresh must never
/// strand stale destinations that later calls then refuse to repair.
#[test]
fn acceptance_competing_refreshes_converge_to_current_publication() {
    use std::sync::Barrier;

    let (mut writer, live) = live_channel();
    let runtime = Arc::new(unconnected_runtime(all_vars()));

    for round in 0..600_u64 {
        let old_audit = format!("{}", 100_000 + round * 2);
        let new_audit = format!("{}", 100_001 + round * 2);
        writer.publish(&SettingsSnapshot {
            revision: (round * 2 + 1) as i64,
            rows: vec![acceptance_row("DISCORD_AUDIT_LOG_CHANNEL_ID", &old_audit)],
        });

        // Three competitors refresh the older publication from a barrier
        // release while this thread publishes and applies the newer one.
        let barrier = Arc::new(Barrier::new(4));
        std::thread::scope(|scope| {
            for _ in 0..3 {
                let (runtime, live, barrier) = (&runtime, &live, &barrier);
                scope.spawn(move || {
                    barrier.wait();
                    runtime.refresh_channels_with(Some(live));
                });
            }
            barrier.wait();
            writer.publish(&SettingsSnapshot {
                revision: (round * 2 + 2) as i64,
                rows: vec![acceptance_row("DISCORD_AUDIT_LOG_CHANNEL_ID", &new_audit)],
            });
            runtime.refresh_channels_with(Some(&live));
        });

        runtime.refresh_channels_with(Some(&live));
        assert_eq!(
            runtime.channels_for_test().audit.as_deref(),
            Some(new_audit.as_str()),
            "round {round}: destinations must converge to the current \
             publication, never strand the older snapshot's channels"
        );
    }
}

/// Control: many concurrent refreshes of one unchanged publication are
/// idempotent and keep the installed destinations exact.
#[test]
fn acceptance_concurrent_refresh_of_one_publication_is_idempotent() {
    let (mut writer, live) = live_channel();
    let runtime = Arc::new(unconnected_runtime(all_vars()));
    writer.publish(&SettingsSnapshot {
        revision: 3,
        rows: vec![acceptance_row("DISCORD_AUDIT_LOG_CHANNEL_ID", "4444")],
    });

    std::thread::scope(|scope| {
        for _ in 0..8 {
            let (runtime, live) = (&runtime, &live);
            scope.spawn(move || {
                for _ in 0..50 {
                    runtime.refresh_channels_with(Some(live));
                }
            });
        }
    });

    let settled = runtime.channels_for_test();
    assert_eq!(settled.audit.as_deref(), Some("4444"));
    assert_eq!(settled.voice.as_deref(), Some(VOICE_CH));
    assert_eq!(settled.moderation.as_deref(), Some(MOD_CH));
    runtime.refresh_channels_with(Some(&live));
    assert_eq!(runtime.channels_for_test(), settled);
}

/// Control: a restored snapshot whose revision is numerically lower than the
/// applied one is legitimate and must be applied — refresh must not grow a
/// monotonic-only revision policy.
#[test]
fn acceptance_restored_lower_revision_snapshot_is_applied() {
    let (mut writer, live) = live_channel();
    let runtime = unconnected_runtime(all_vars());

    writer.publish(&SettingsSnapshot {
        revision: 9,
        rows: vec![acceptance_row("DISCORD_AUDIT_LOG_CHANNEL_ID", "4444")],
    });
    runtime.refresh_channels_with(Some(&live));
    assert_eq!(runtime.channels_for_test().audit.as_deref(), Some("4444"));

    writer.publish(&SettingsSnapshot {
        revision: 4,
        rows: vec![acceptance_row("DISCORD_AUDIT_LOG_CHANNEL_ID", "5555")],
    });
    runtime.refresh_channels_with(Some(&live));
    assert_eq!(
        runtime.channels_for_test().audit.as_deref(),
        Some("5555"),
        "a restored lower-revision snapshot must reload, not be rejected"
    );

    // And forward again, so neither direction is treated as one-way.
    writer.publish(&SettingsSnapshot {
        revision: 9,
        rows: vec![acceptance_row("DISCORD_AUDIT_LOG_CHANNEL_ID", "4444")],
    });
    runtime.refresh_channels_with(Some(&live));
    assert_eq!(runtime.channels_for_test().audit.as_deref(), Some("4444"));
}

/// Controls: malformed last-good, blank-disable and deployment fallback keep
/// their documented behavior under repeated current-publication refreshes,
/// and each state can still be left by a later coherent publication.
#[test]
fn acceptance_malformed_blank_and_fallback_controls_hold() {
    let (mut writer, live) = live_channel();
    let runtime = unconnected_runtime(all_vars());

    // No live reader at all: boot deployment destinations stand.
    runtime.refresh_channels_with(None);
    assert_eq!(runtime.channels_for_test(), channels());

    // Malformed stored row: last good destinations stand, repeatedly.
    writer.publish(&SettingsSnapshot {
        revision: 1,
        rows: vec![acceptance_row(
            "DISCORD_VOICE_LOG_CHANNEL_ID",
            "not-a-snowflake",
        )],
    });
    runtime.refresh_channels_with(Some(&live));
    runtime.refresh_channels_with(Some(&live));
    assert_eq!(runtime.channels_for_test(), channels());

    // A later valid publication recovers from the malformed one.
    writer.publish(&SettingsSnapshot {
        revision: 2,
        rows: vec![acceptance_row("DISCORD_VOICE_LOG_CHANNEL_ID", "6666")],
    });
    runtime.refresh_channels_with(Some(&live));
    assert_eq!(runtime.channels_for_test().voice.as_deref(), Some("6666"));

    // Blanking every destination parks the mirror, repeatedly.
    writer.publish(&SettingsSnapshot {
        revision: 3,
        rows: vec![
            acceptance_row("DISCORD_AUDIT_LOG_CHANNEL_ID", ""),
            acceptance_row("DISCORD_VOICE_LOG_CHANNEL_ID", ""),
            acceptance_row("DISCORD_MODERATION_LOG_CHANNEL_ID", ""),
        ],
    });
    runtime.refresh_channels_with(Some(&live));
    runtime.refresh_channels_with(Some(&live));
    assert_eq!(runtime.channels_for_test(), AuditChannelIds::default());

    // Deleting the blanking rows restores the deployment fallback.
    writer.publish(&SettingsSnapshot {
        revision: 4,
        rows: vec![],
    });
    runtime.refresh_channels_with(Some(&live));
    assert_eq!(runtime.channels_for_test(), channels());
}

// ----------------------------------------------- drain-report outcomes --

/// `ScanBoundReached` and `Unclaimed` deliveries report through the outcome
/// counter without quarantine: executes the bounded-scan arm of `log_report`
/// and asserts the counter mapping.
#[test]
fn log_report_counts_scan_bound_reached_and_unclaimed() {
    let report = DrainReport {
        deliveries: vec![
            (
                "scan-bound-entry".to_owned(),
                DeliverOutcome::ScanBoundReached,
            ),
            ("unclaimed-entry".to_owned(), DeliverOutcome::Unclaimed),
        ],
        failed: vec![],
    };
    let capture = sweep_log_capture();
    let start = capture.text().len();
    log_report(&report);
    let logs = capture.text().split_off(start);
    assert!(logs.contains("rows=2"), "report counts both rows");
    assert!(
        logs.contains("scan_bound_reached=1"),
        "report counts the bound hit"
    );
    assert!(logs.contains("unclaimed=1"), "report counts the refusal");
}

// ------------------------------------------ protected cursor budget tests --

/// Freeze wall time for the entire real job, including every SQL await. A
/// runnable task prevents Tokio's idle-time advance while SQL is in flight.
/// Only a deliberately pending history read parks that task, letting the
/// actual scan/outer timers fire; cancelling the read makes it runnable again
/// before checkpoint/release SQL. Neither deadline is replaced by a test seam.
struct CursorReadClock {
    runnable: watch::Sender<bool>,
    driver: tokio::task::JoinHandle<()>,
}

impl CursorReadClock {
    fn pause() -> Self {
        tokio::time::pause();
        let (runnable, mut enabled) = watch::channel(true);
        let driver = tokio::spawn(async move {
            loop {
                let run = *enabled.borrow_and_update();
                if run {
                    tokio::task::yield_now().await;
                } else if enabled.changed().await.is_err() {
                    break;
                }
            }
        });
        Self { runnable, driver }
    }
}

impl Drop for CursorReadClock {
    fn drop(&mut self) {
        self.driver.abort();
        tokio::time::resume();
    }
}

/// Drop runs inside the real history timeout, before the service starts SQL.
/// Waking the parked driver here forbids another automatic clock jump during
/// that SQL, even if the database takes longer than the checkpoint reserve.
struct CursorHistoryWait(watch::Sender<bool>);

impl CursorHistoryWait {
    fn new(runnable: watch::Sender<bool>) -> Self {
        runnable.send_replace(false);
        Self(runnable)
    }
}

impl Drop for CursorHistoryWait {
    fn drop(&mut self) {
        self.0.send_replace(true);
    }
}

#[derive(Default)]
struct PacedHistory {
    scanning: bool,
    shared_batch: bool,
    clock: Option<watch::Sender<bool>>,
    requests: Vec<(String, Option<String>)>,
    markers: HashMap<String, MirrorMessage>,
}

#[derive(Clone, Default)]
struct PacedCursorMirror {
    history: Arc<Mutex<PacedHistory>>,
    posts: Arc<Mutex<Vec<String>>>,
}

impl AuditMirror for PacedCursorMirror {
    async fn post_mirror_checked<Fut, E>(
        &self,
        channel: &str,
        _content: &str,
        _nonce: &str,
        authorize: Fut,
    ) -> Result<Result<String, MirrorError>, E>
    where
        Fut: std::future::Future<Output = Result<(), E>> + Send,
        E: Send,
    {
        authorize.await?;
        self.posts.lock().unwrap().push(channel.to_owned());
        Ok(Err(MirrorError::Uncertain(
            "accepted but timed out".to_owned(),
        )))
    }

    async fn channel_document(&self, channel: &str) -> Result<MirrorChannel, MirrorError> {
        assert!([AUDIT_CH, VOICE_CH].contains(&channel));
        Ok(private_document())
    }

    async fn channel_history(&self, channel: &str, before: Option<&str>, limit: u8) -> PageResult {
        assert_eq!(limit, 100);
        let (shared, clock) = {
            let mut history = self.history.lock().unwrap();
            history
                .requests
                .push((channel.to_owned(), before.map(str::to_owned)));
            if !history.scanning {
                return Ok(vec![]);
            }
            if let Some(marker) = history.markers.get(channel) {
                return Ok(vec![marker.clone()]);
            }
            (history.shared_batch, history.clock.clone().unwrap())
        };
        let newest = match channel {
            AUDIT_CH => 900_000_u64,
            VOICE_CH => 800_000,
            _ => panic!("unexpected synthetic channel"),
        };
        let top = before.map_or(newest, |id| id.parse::<u64>().unwrap() - 1);
        let page = (newest - top) / 100;
        let stall = if shared {
            channel == VOICE_CH && page == 1
        } else {
            page == 10
        };
        if stall {
            // Auto-advance is safe only while this read is pending: there is
            // no SQL until its actual deadline cancels it. Without the reserve
            // (or the shared budget), the real outer timeout cancels the job.
            let _wait = CursorHistoryWait::new(clock);
            return std::future::pending().await;
        }
        if page < 10 || channel == VOICE_CH {
            tokio::time::advance(Duration::from_secs(2)).await;
        }
        Ok((0..100_u64)
            .map(|offset| MirrorMessage {
                id: (top - offset).to_string(),
                author_id: "1234".to_owned(),
                content: "unrelated channel noise".to_owned(),
            })
            .collect())
    }
}

fn paced_runtime(
    pool: &PgPool,
    mirror: &PacedCursorMirror,
) -> Arc<AuditRuntime<PacedCursorMirror>> {
    let (pool, mirror) = (pool.clone(), mirror.clone());
    Arc::new(
        AuditRuntime::new(
            all_vars(),
            GUILD.to_owned(),
            Box::new(move || {
                let parts = Parts {
                    pool: pool.clone(),
                    mirror: mirror.clone(),
                    bot_user_id: BOT.to_owned(),
                };
                Box::pin(async move { Ok(parts) })
            }),
        )
        .unwrap(),
    )
}

async fn cursor_budget_case(shared: bool, delay_halt: bool) {
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("guarded test bootstrap required");
    let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("create disposable runtime cursor fixture");
    let mirror = PacedCursorMirror::default();
    let runtime = paced_runtime(db.pool(), &mirror);
    let mut entries = vec![("paced-audit", AUDIT_CH, event("paced-audit"))];
    if shared {
        let mut voice = event("paced-voice");
        voice.kind = AuditKind::VoiceJoin;
        voice.channel = two_bot_core::audit::AuditChannel::Voice;
        entries.push(("paced-voice", VOICE_CH, voice));
    }
    for (_, channel, event) in &entries {
        assert_eq!(
            runtime.record(event).await.unwrap(),
            RecordOutcome::Queued {
                mirror_channel_id: (*channel).to_owned()
            }
        );
    }
    let seeded = drained(&runtime).await;
    assert_eq!(seeded.len(), entries.len());
    assert!(seeded
        .iter()
        .all(|(_, result)| *result == DeliverOutcome::Ambiguous));
    let store = AuditStore::new(db.pool());
    let mut originals = Vec::new();
    for (entry, _, _) in &entries {
        originals.push(store.get(entry).await.unwrap().unwrap());
        sqlx::query("UPDATE operational_audit_log SET delivery_lease_until = clock_timestamp() - interval '1 second' WHERE entry_id = $1")
            .bind(entry).execute(db.pool()).await.unwrap();
    }
    // A later fresh row is not even claimed once the shared deadline has
    // elapsed. Its entire authoritative row must remain untouched.
    let untouched =
        if shared {
            runtime.record(&event("after-budget")).await.unwrap();
            Some(sqlx::query_scalar::<_, serde_json::Value>(
            "SELECT to_jsonb(a) FROM operational_audit_log a WHERE entry_id = 'after-budget'",
        ).fetch_one(db.pool()).await.unwrap())
        } else {
            None
        };
    // The delayed variant holds only this disposable fixture's halt table.
    // A real, successful server-side wait releases it below, within the
    // fixture's five-second statement allowance; no production seam changes.
    let halt_lock = if delay_halt {
        let mut tx = db.pool().begin().await.unwrap();
        sqlx::query("LOCK TABLE audit_kill_switch IN ACCESS EXCLUSIVE MODE")
            .execute(&mut *tx)
            .await
            .unwrap();
        Some(tx)
    } else {
        None
    };
    let clock = CursorReadClock::pause();
    {
        let mut history = mirror.history.lock().unwrap();
        history.scanning = true;
        history.shared_batch = shared;
        history.requests.clear();
        history.clock = Some(clock.runnable.clone());
    }
    let (_stop, shutdown) = watch::channel(false);
    let job = retry_job(runtime.clone(), shutdown, Duration::ZERO);
    // Same timeout wrapper as the production supervisor, using the actual
    // retry_job action -> runtime sweep -> service drain_pending path.
    let sweep = tokio::time::timeout(job.timeout, (job.action)());
    let outcome = if let Some(mut tx) = halt_lock {
        let started = tokio::time::Instant::now();
        let (outcome, ()) = tokio::join!(
            biased;
            sweep,
            async {
                sqlx::query("SELECT pg_sleep(1.5)")
                    .execute(&mut *tx)
                    .await
                    .unwrap();
                assert_eq!(
                    tokio::time::Instant::now(), started,
                    "successful SQL wall time must not consume the logical budget"
                );
                assert!(mirror.history.lock().unwrap().requests.is_empty());
                tx.rollback().await.unwrap();
            }
        );
        outcome
    } else {
        sweep.await
    };
    drop(clock);
    let requests = mirror.history.lock().unwrap().requests.clone();
    let mut bounded = Vec::new();
    for (entry, _, _) in &entries {
        let row = store.get(entry).await.unwrap().unwrap();
        let released: bool = sqlx::query_scalar("SELECT delivery_state = 'pending' AND delivery_claim_token IS NULL AND delivery_lease_until IS NULL FROM operational_audit_log WHERE entry_id = $1")
            .bind(entry).fetch_one(db.pool()).await.unwrap();
        bounded.push((row, released));
        // Also permit observation of restart behavior on the broken baseline;
        // release itself was already captured above, not repaired as evidence.
        sqlx::query("UPDATE operational_audit_log SET delivery_lease_until = clock_timestamp() - interval '1 second' WHERE entry_id = $1 AND delivery_lease_until IS NOT NULL")
            .bind(entry).execute(db.pool()).await.unwrap();
    }
    let untouched_after = if shared {
        let after = sqlx::query_scalar::<_, serde_json::Value>(
            "SELECT to_jsonb(a) FROM operational_audit_log a WHERE entry_id = 'after-budget'",
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        // Hold this fixture row out of the independent restart below. The
        // comparison captures the production sweep before this test-only claim.
        let _held = store.claim("after-budget").await.unwrap();
        Some(after)
    } else {
        None
    };
    drop((job, runtime));
    let peer = db.independent_pool().await.unwrap();
    {
        let mut history = mirror.history.lock().unwrap();
        history.requests.clear();
        history.markers.insert(
            AUDIT_CH.to_owned(),
            marked(
                if shared { "898400" } else { "899000" },
                &event("paced-audit"),
            ),
        );
        if shared {
            let mut voice = event("paced-voice");
            voice.kind = AuditKind::VoiceJoin;
            voice.channel = two_bot_core::audit::AuditChannel::Voice;
            history
                .markers
                .insert(VOICE_CH.to_owned(), marked("799900", &voice));
        }
    }
    let restarted = paced_runtime(&peer, &mirror);
    let recovered = drained(&restarted).await;
    let restart_requests = mirror.history.lock().unwrap().requests.clone();
    let posts = mirror.posts.lock().unwrap().clone();
    drop(restarted);
    peer.close().await;
    db.close().await.unwrap();

    assert!(
        matches!(outcome, Ok(Ok(()))),
        "sweep must checkpoint before outer cancellation: {outcome:?}"
    );
    assert!(
        untouched_after == untouched,
        "deadline must prevent starting a later row"
    );
    let audit_pages = if shared { 16 } else { 11 };
    let mut expected: Vec<_> = (0..audit_pages)
        .map(|n| {
            (
                AUDIT_CH.to_owned(),
                if n == 0 {
                    None
                } else {
                    Some((900_001 - n * 100).to_string())
                },
            )
        })
        .collect();
    if shared {
        expected.extend([
            (VOICE_CH.to_owned(), None),
            (VOICE_CH.to_owned(), Some("799901".to_owned())),
        ]);
    }
    assert_eq!(
        requests, expected,
        "only completed pages can advance the cursor"
    );
    let audit_floor = if shared { "898401" } else { "899001" };
    let mut expected_restart = vec![(AUDIT_CH.to_owned(), Some(audit_floor.to_owned()))];
    if shared {
        expected_restart.push((VOICE_CH.to_owned(), Some("799901".to_owned())));
    }
    assert_eq!(
        restart_requests, expected_restart,
        "independent owners resume at the exact saved floors"
    );
    for (n, (row, released)) in bounded.iter().enumerate() {
        assert!(
            *released,
            "bounded scan must release before the outer timeout"
        );
        assert_eq!(
            row.reconcile_before.as_deref(),
            Some(if n == 0 { audit_floor } else { "799901" })
        );
        assert_eq!(row.attempts, 1);
        assert_eq!(row.search_before, originals[n].search_before);
        assert_eq!(row.nonce, originals[n].nonce);
        assert_eq!(row.attempted_at, originals[n].attempted_at);
    }
    assert_eq!(
        posts,
        entries
            .iter()
            .map(|(_, channel, _)| (*channel).to_owned())
            .collect::<Vec<_>>(),
        "reconcile cannot POST again"
    );
    assert_eq!(recovered.len(), entries.len());
    for (entry, result) in recovered {
        let id = if entry == "paced-voice" {
            "799900"
        } else if shared {
            "898400"
        } else {
            "899000"
        };
        assert_eq!(
            result,
            DeliverOutcome::Reconciled {
                message_id: id.to_owned()
            }
        );
    }
}

/// Default 30-second reconcile scanning must yield before the production
/// 25-second job timeout; otherwise ten validated pages are lost on cancel.
#[tokio::test]
async fn retry_job_checkpoints_progress_before_its_production_timeout() {
    cursor_budget_case(false, false).await;
}

/// A 20-second first row consumes the same sweep budget as the later row.
/// Giving each row a fresh timeout loses the later row's validated floor.
#[tokio::test]
async fn retry_job_later_rows_share_the_remaining_sweep_budget() {
    cursor_budget_case(true, false).await;
}

/// A successful 1.5-second halt-table wait must not alter either exact page
/// sequence, saved cursor, restart, untouched-row or single-POST observation.
#[tokio::test]
async fn retry_job_cursor_budget_excludes_successful_sql_wall_time() {
    for shared in [false, true] {
        cursor_budget_case(shared, true).await;
    }
}

#[tokio::test]
async fn cursor_budget_clock_restores_time_when_its_action_is_cancelled() {
    let (ready, wait) = tokio::sync::oneshot::channel();
    let action = tokio::spawn(async move {
        let clock = CursorReadClock::pause();
        let _wait = CursorHistoryWait::new(clock.runnable.clone());
        ready.send(()).unwrap();
        std::future::pending::<()>().await;
    });
    wait.await.unwrap();
    action.abort();
    assert!(action.await.unwrap_err().is_cancelled());
    // pause() would panic if cancellation had left the runtime clock paused.
    drop(CursorReadClock::pause());
}
