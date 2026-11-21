#![cfg(test)]
//! Receiver boundary plus the real single-attempt adapter and durable send lane.
//! Only guarded TestDatabase pools and an ephemeral loopback Discord double.
use super::*;
use axum::{extract::State, serve::ListenerExt};
use std::sync::atomic::AtomicBool;
use tokio::{
    sync::Notify,
    time::{timeout_at, Instant},
};
use two_bot_core::{
    internal_actions::VoiceMoveOutcome,
    send_admission::{AdmissionError, ClaimFuture, PgSendAdmission, SendAdmission, TokenKey},
    voice_move_decision::VoiceMoveDecision,
};
use two_bot_discord::{
    executor::member::VoiceMoveReadError::Discord as VoiceReadError,
    internal_actions::CooldownGovernor, ActionExecutor, DiscordError,
};

struct MockDiscord {
    origin: String,
    requests: Arc<Mutex<Vec<Value>>>,
    sockets: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

#[derive(Clone)]
struct MockState {
    requests: Arc<Mutex<Vec<Value>>>,
    replies: Arc<Vec<(StatusCode, Value)>>,
    pause: Vec<(usize, Arc<Pause>)>,
}

impl MockDiscord {
    async fn start(replies: Vec<(StatusCode, Value)>) -> Self {
        Self::start_with_pauses(replies, vec![]).await
    }

    async fn start_with_pauses(
        replies: Vec<(StatusCode, Value)>,
        pause: Vec<(usize, Arc<Pause>)>,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let app = Router::new()
            .fallback(
                |State(state): State<MockState>, request: Request| async move {
                    let (parts, body) = request.into_parts();
                    let raw = to_bytes(body, 8192).await.unwrap();
                    let body: Value = serde_json::from_slice(&raw).unwrap_or(Value::Null);
                    let index = {
                        let mut seen = state.requests.lock().unwrap();
                        let index = seen.len();
                        seen.push(json!({
                            "method": parts.method.as_str(),
                            "path": parts.uri.path(),
                            "body": body,
                        }));
                        index
                    };
                    if let Some((_, pause)) = state.pause.iter().find(|(at, _)| *at == index) {
                        pause.wait().await;
                    }
                    let (status, body) = state.replies[index.min(state.replies.len() - 1)].clone();
                    (status, [(header::CONNECTION, "close")], Json(body))
                },
            )
            .with_state(MockState {
                requests: requests.clone(),
                replies: Arc::new(replies),
                pause,
            });
        let sockets = Arc::new(AtomicUsize::new(0));
        let accepted = sockets.clone();
        let listener = listener.tap_io(move |_| {
            accepted.fetch_add(1, Ordering::SeqCst);
        });
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            origin,
            requests,
            sockets,
            task,
        }
    }

    fn count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }

    fn assert_wires(&self, count: usize) {
        assert_eq!(
            (self.count(), self.sockets.load(Ordering::SeqCst)),
            (count, count)
        );
    }

    fn assert_patch_indices(&self, patches: &[usize]) {
        for (index, request) in self.requests.lock().unwrap().iter().enumerate() {
            let method = if patches.contains(&index) {
                "PATCH"
            } else {
                "GET"
            };
            assert_eq!(request["method"], method, "request {index}");
        }
    }
}

impl Drop for MockDiscord {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn token() -> String {
    format!("local-receiver-fixture-{}", std::process::id())
}

fn adapter(pool: sqlx::PgPool) -> AnnouncementExecutor {
    let _ = rustls::crypto::ring::default_provider().install_default();
    AnnouncementExecutor::with_admission(
        Arc::new(twilight_http::Client::new(token())),
        config().channel_keys().clone(),
        CooldownGovernor::new(),
        Arc::new(PgSendAdmission::new(pool, &token()).unwrap()),
    )
    .unwrap()
}

fn app(pool: sqlx::PgPool, mock: &MockDiscord) -> Router {
    let effect = adapter(pool.clone())
        .with_loopback_test_origin(&mock.origin)
        .unwrap();
    // Announcement-only harness: membership, event.read, event-mutation, the
    // member-moderation verbs and the channel-moderation verbs never arrive
    // here, so the doubles only assert they stay uncalled.
    let member = Arc::new(MockEffect::new(MockOutcome::Success));
    router(Arc::new(ReceiverState::new(
        config(),
        pool,
        Arc::new(effect),
        member,
        Arc::new(MockEventRead::default()),
        Arc::new(MockEventMutate),
        Arc::new(MockModeration::default()),
        Arc::new(MockChannel::default()),
    )))
}

#[tokio::test]
async fn loopback_seam_refuses_public_origins_and_missing_admission() {
    for origin in [
        "https://discord.com",
        "http://192.0.2.1:8091",
        "http://127.0.0.1:8091/api/v10",
        "http://fixture@127.0.0.1:8091",
        "http://127.0.0.1:8091?redirect=elsewhere",
    ] {
        assert!(adapter(lazy_pool())
            .with_loopback_test_origin(origin)
            .is_err());
    }
    let unadmitted = AnnouncementExecutor::new(
        Arc::new(twilight_http::Client::new(token())),
        config().channel_keys().clone(),
        CooldownGovernor::new(),
    );
    assert!(unadmitted
        .with_loopback_test_origin("http://127.0.0.1:8091")
        .is_err());
}

#[tokio::test]
async fn real_adapter_success_and_429_replay_preserve_cross_pool_shared_hold() {
    let Some(db) = database().await else { return };
    let mock = MockDiscord::start(vec![
        (
            StatusCode::OK,
            json!({"id":"444444444444444444", "channel_id":"333333333333333333"}),
        ),
        (
            StatusCode::TOO_MANY_REQUESTS,
            json!({"retry_after":120, "global":false}),
        ),
    ])
    .await;
    let first_app = app(db.pool().clone(), &mock);
    let (status, _, first) = answer(
        first_app.clone(),
        signed(payload(), "old", "intent-real-success"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(first["result"]["message_id"], "444444444444444444");
    {
        let requests = mock.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0]["method"], "POST");
        assert_eq!(
            requests[0]["path"],
            "/api/v10/channels/333333333333333333/messages"
        );
        // Kit embed treatment: the validated text renders as one branded
        // embed — ember default, Announcements surface footer — with no
        // plain content (colour + footer are asserted discord-side).
        assert!(requests[0]["body"].get("content").is_none());
        assert_eq!(
            requests[0]["body"]["embeds"][0]["title"],
            "fixture announcement"
        );
        assert_eq!(requests[0]["body"]["allowed_mentions"]["parse"], json!([]));
    }
    let (status, _, refused) = answer(
        first_app,
        signed(payload(), "old", "intent-real-rate-limit"),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(refused["error"]["code"], "discord_rejected");
    assert_eq!(refused["error"]["retryable"], false);
    assert_eq!(mock.count(), 2, "429 must not be retried internally");

    // New pools, stores, adapter and local governor: only the durable lane and
    // receipts survive. Key rotation must not grant a second execution.
    let second = db.independent_pool().await.unwrap();
    let restarted = app(second.clone(), &mock);
    for (intent, code) in [
        ("intent-real-success", None),
        ("intent-real-rate-limit", Some("discord_rejected")),
    ] {
        let (_, headers, replay) =
            answer(restarted.clone(), signed(payload(), "new", intent)).await;
        assert_eq!(headers["idempotent-replay"], "true");
        if let Some(code) = code {
            assert_eq!(replay["error"]["code"], code);
        } else {
            assert_eq!(replay["result"], first["result"]);
        }
    }
    let (_, _, independent) = answer(
        restarted,
        signed(payload(), "new", "intent-real-independent"),
    )
    .await;
    assert_eq!(independent["error"]["code"], "no_effect");
    assert_eq!(independent["error"]["retryable"], false);
    let held: bool = sqlx::query_scalar(
        "SELECT NOT in_flight AND hold_until_ms > \
         floor(extract(epoch FROM clock_timestamp()) * 1000)::bigint \
         FROM discord_send_admission",
    )
    .fetch_one(&second)
    .await
    .unwrap();
    assert!(held);
    let other = ActionExecutor::with_admission(
        token(),
        Some(mock.origin.clone()),
        Arc::new(PgSendAdmission::new(second.clone(), &token()).unwrap()),
    )
    .unwrap();
    assert!(matches!(
        other
            .ban("111111111111111111", "222222222222222222", "fixture")
            .await,
        Err(DiscordError::Unavailable(_))
    ));
    assert_eq!(
        mock.count(),
        2,
        "independent and cross-transport sends stay held"
    );
    let completed: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM internal_idempotency WHERE state = 'completed'")
            .fetch_one(&second)
            .await
            .unwrap();
    assert_eq!(completed, 3);
    second.close().await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn real_adapter_invalid_receipt_keeps_intent_and_restart_send_lane_occupied() {
    let Some(db) = database().await else { return };
    let mock = MockDiscord::start(vec![(
        StatusCode::OK,
        json!({"id":"444444444444444444", "channel_id":"555555555555555555"}),
    )])
    .await;
    let (_, _, first) = answer(
        app(db.pool().clone(), &mock),
        signed(payload(), "old", "intent-real-uncertain"),
    )
    .await;
    assert_eq!(first["error"]["code"], "needs_reconciliation");
    assert_eq!(first["error"]["retryable"], false);
    let second = db.independent_pool().await.unwrap();
    let restarted = app(second.clone(), &mock);
    let (_, headers, replay) = answer(
        restarted.clone(),
        signed(payload(), "new", "intent-real-uncertain"),
    )
    .await;
    assert_eq!(replay["error"]["code"], "needs_reconciliation");
    assert!(!headers.contains_key("idempotent-replay"));
    let (_, _, independent) = answer(
        restarted,
        signed(payload(), "new", "intent-real-after-uncertain"),
    )
    .await;
    assert_eq!(independent["error"]["code"], "no_effect");
    let occupied: bool = sqlx::query_scalar("SELECT in_flight FROM discord_send_admission")
        .fetch_one(&second)
        .await
        .unwrap();
    assert!(occupied);
    assert_eq!(
        mock.count(),
        1,
        "uncertain effects never grant another send"
    );
    second.close().await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn voice_evidence_checks_member_identity_and_preserves_read_errors() {
    let guild = staging_guild();
    let _ = rustls::crypto::ring::default_provider().install_default();
    let allowed = Some(VoiceMoveDecision::Proceed { from_channel_id: 1 });
    let denied = Some(VoiceMoveDecision::Refuse(VoiceMoveOutcome::NotPermitted));
    let active = json!("2099-01-01T00:00:00.000000+00:00");
    for (status, id, timeout, expected) in [
        (StatusCode::OK, "3", Some(Value::Null), allowed),
        (StatusCode::OK, "3", Some(active), denied),
        (StatusCode::OK, "3", None, None),
        (StatusCode::OK, "9", Some(Value::Null), None),
        (StatusCode::FORBIDDEN, "3", Some(Value::Null), None),
    ] {
        let (mut member, mut bot) = (voice_member(id, false), voice_member("4", true));
        bot["roles"] = json!(["5"]);
        if let Some(timeout) = timeout {
            member["communication_disabled_until"] = timeout;
        }
        let mock = MockDiscord::start(vec![
            (StatusCode::OK, json!({"id":"4","bot":true})),
            (
                StatusCode::OK,
                json!({"id": guild, "owner_id": "6", "afk_channel_id": null,
                "roles": [{"id": guild, "position": 0, "permissions": "1049600"},
                          {"id": "5", "position": 1, "permissions": "16777216"}]}),
            ),
            (StatusCode::OK, voice_channel("2", 2, "target", 0, 3)),
            (status, member),
            (StatusCode::OK, voice_channel("1", 2, "source", 0, 0)),
            (StatusCode::OK, bot),
        ])
        .await;
        let api = ActionExecutor::with_proxy(token(), Some(mock.origin.clone())).unwrap();
        let deadline = Instant::now() + Duration::from_secs(4);
        let target = api
            .voice_move_target_until(guild.parse().unwrap(), 2, deadline)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(target.user_limit(), 3);
        let result = api.voice_move_evidence_until(target, 3, 1, deadline).await;
        match (status, expected, result) {
            (_, Some(expected), Ok(evidence)) => assert_eq!(evidence.decide(1), expected),
            (StatusCode::OK, None, Err(VoiceReadError(DiscordError::Unavailable(detail)))) => {
                assert_eq!(detail, "voice_move_evidence_incomplete");
            }
            (StatusCode::FORBIDDEN, None, Err(VoiceReadError(DiscordError::Rejected(_)))) => {}
            _ => panic!("unexpected voice evidence result"),
        }
        let requests = mock.requests.lock().unwrap();
        assert_eq!(requests.len(), if expected.is_some() { 6 } else { 4 });
        assert!(requests.iter().all(|request| request["method"] == "GET"));
    }
}

#[tokio::test]
async fn governed_voice_read_cutoff_leaves_the_real_lane_reusable() {
    let Some(db) = database().await else { return };
    let stalled = Arc::new(Pause::default());
    let mock = MockDiscord::start_with_pauses(
        vec![(StatusCode::OK, Value::Null)],
        vec![(0, stalled.clone())],
    )
    .await;
    let _ = rustls::crypto::ring::default_provider().install_default();
    let lane = Arc::new(PgSendAdmission::new(db.pool().clone(), &token()).unwrap());
    let api =
        ActionExecutor::with_admission(token(), Some(mock.origin.clone()), lane.clone()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    let read = api.voice_move_target_until(staging_guild().parse().unwrap(), 2, deadline);
    let (result, ()) = tokio::join!(timeout_at(deadline + Duration::from_secs(1), read), async {
        timeout_at(deadline, stalled.entered.notified())
            .await
            .unwrap();
        assert!(matches!(
            timeout_at(deadline, lane.admit_strict()).await,
            Ok(Err(AdmissionError::Blocked))
        ));
    });
    assert!(matches!(
        result,
        Ok(Err(VoiceReadError(DiscordError::Timeout)))
    ));
    assert_eq!((mock.count(), mock.sockets.load(Ordering::SeqCst)), (1, 1));
    assert_eq!(mock.requests.lock().unwrap()[0]["method"], "GET");
    let permit = lane.admit_strict().await.unwrap();
    permit.complete(None).await.unwrap();
    db.close().await.unwrap();
}

#[derive(Debug, Default)]
struct Pause {
    entered: Notify,
    resume: Notify,
}

impl Pause {
    async fn wait(&self) {
        self.entered.notify_one();
        self.resume.notified().await;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MoveVeto {
    Capacity,
    Readiness,
    Source,
    PolicyDuringReads(u32),
    MemberRole,
    MemberTimeout,
    MessageRole,
    Cutoff,
}

/// Pause a governed read or the real write claim before or after SQL.
#[derive(Debug)]
struct PausedAdmission(PgSendAdmission, Mutex<Option<MoveVeto>>, Pause);

impl SendAdmission for PausedAdmission {
    fn token_key(&self) -> &TokenKey {
        self.0.token_key()
    }
    fn admit(&self) -> ClaimFuture<'_> {
        self.0.admit()
    }
    fn admit_until(&self, at: Instant) -> ClaimFuture<'_> {
        Box::pin(async move {
            let pause = self
                .1
                .lock()
                .unwrap()
                .take_if(|veto| {
                    let MoveVeto::PolicyDuringReads(remaining) = veto else {
                        return false;
                    };
                    *remaining -= 1;
                    *remaining == 0
                })
                .is_some();
            if pause {
                self.2.wait().await;
            }
            self.0.admit_until(at).await
        })
    }
    fn admit_strict(&self) -> ClaimFuture<'_> {
        Box::pin(async {
            let pause = self.1.lock().unwrap().take();
            if pause == Some(MoveVeto::Cutoff) {
                self.2.wait().await;
            }
            let permit = self.0.admit_strict().await?;
            if pause.is_some_and(|veto| veto != MoveVeto::Cutoff) {
                self.2.wait().await;
            }
            Ok(permit)
        })
    }
}

const FIRST: &str = "41";
const SECOND: &str = "42";
const SOURCE: &str = "31";
const TARGET: &str = "35";
type MovePipeline = two_bot_discord::OrderedLevelingPipeline<two_bot_core::MemStore>;

fn move_facts([member, source, target]: [&str; 3]) -> [(StatusCode, Value); 6] {
    let guild = staging_guild();
    let mut member = voice_member(member, false);
    member["communication_disabled_until"] = Value::Null;
    let mut bot = voice_member("43", true);
    bot["roles"] = json!(["44"]);
    [json!({"id":"43","bot":true}),
        json!({"id":guild,"owner_id":"45","afk_channel_id":null,"roles":[{"id":guild,"permissions":"1049600","position":0},{"id":"44","permissions":"16777216","position":1}]}),
        voice_channel(target, 2, "target", 0, 1), member,
        voice_channel(source, 2, "source", 0, 0), bot,
    ].map(|body| (StatusCode::OK, body))
}

async fn seed_move_pipeline(
    cache: Arc<two_bot_discord::SharedCache>,
    clock: Arc<VoiceOccupancyClock>,
) -> MovePipeline {
    let pipeline = MovePipeline::with_shared_cache(
        two_bot_core::MemStore::new(),
        None,
        two_bot_discord::NoInvites,
        two_bot_discord::PipelineSnapshots::new(),
        cache,
    );
    pipeline.set_voice_transition_observer(clock);
    pipeline.handle(&seeded_guild_event()).await.unwrap();
    pipeline
}

async fn publish_move_room(pipeline: &MovePipeline, member: &str, room: Option<&str>) {
    let event = Event::VoiceStateUpdate(Box::new(voice_state(member, room)));
    pipeline.handle(&event).await.unwrap();
}

#[tokio::test]
async fn governed_voice_move_checks_the_ordered_roster_and_reuses_unsent_lane() {
    use two_bot_discord::executor::member::VoiceMoveRoster;
    use MoveVeto::{
        Capacity, Cutoff, MemberRole, MemberTimeout, MessageRole, PolicyDuringReads, Readiness,
        Source,
    };
    let vetoes = [
        (Capacity, 6),
        (Readiness, 6),
        (Source, 6),
        (PolicyDuringReads(2), 3),
        (PolicyDuringReads(6), 6),
        (MemberTimeout, 6),
        (MemberRole, 6),
        (MessageRole, 6),
        (Cutoff, 6),
    ];
    let Some(db) = database().await else { return };
    let _ = rustls::crypto::ring::default_provider().install_default();
    let lane = Arc::new(PausedAdmission(
        PgSendAdmission::new(db.pool().clone(), &token()).unwrap(),
        Mutex::new(None),
        Pause::default(),
    ));
    // The vetoes are read-only; then a stale PATCH, six fresh GETs and a limited PATCH.
    let facts = || move_facts([FIRST, SOURCE, TARGET]);
    let mut replies = vetoes
        .iter()
        .flat_map(|&(veto, reads)| {
            facts()
                .into_iter()
                .take(reads)
                .chain((veto == Capacity).then(&facts).into_iter().flatten())
        })
        .collect::<Vec<_>>();
    replies.extend(facts());
    let first_patch = replies.len();
    replies.push((StatusCode::BAD_REQUEST, Value::Null));
    replies.extend(facts());
    let after_refresh = replies.len();
    replies.extend(facts());
    let second_patch = replies.len();
    replies.push((StatusCode::TOO_MANY_REQUESTS, json!({"retry_after":1.0})));
    let mock = MockDiscord::start(replies).await;
    let api =
        ActionExecutor::with_admission(token(), Some(mock.origin.clone()), lane.clone()).unwrap();
    let cache = voice_snapshot::new_shared_cache();
    let clock = voice_snapshot::VoiceOccupancyClock::new();
    let pipeline = seed_move_pipeline(cache.clone(), clock.clone()).await;
    let change_room = |member, room| publish_move_room(&pipeline, member, room);
    let turn = tokio::sync::Mutex::new(());
    let ready = AtomicBool::new(true);
    let attempt = |deadline| {
        api.move_with_voice_evidence_until(
            [staging_guild(), FIRST, TARGET],
            VoiceMoveRoster::new(&cache, &clock.move_holds, &turn),
            deadline,
            || ready.load(Ordering::SeqCst).then_some(()),
        )
    };
    // A busy guild turn consumes the caller's deadline, never the send lane.
    let busy_turn = turn.lock().await;
    let blocked = attempt(Instant::now() + Duration::from_millis(30)).await;
    assert_eq!(blocked.outcome, VoiceMoveOutcome::DiscordUnavailable);
    mock.assert_wires(0);
    drop(busy_turn);
    let mut total_reads = 0;
    for (veto, expected_reads) in vetoes {
        *lane.1.lock().unwrap() = Some(veto);
        let deadline = Instant::now() + Duration::from_secs(if veto == Cutoff { 2 } else { 5 });
        let mut pending = std::pin::pin!(attempt(deadline));
        tokio::select! {
            () = lane.2.entered.notified() => {},
            _ = pending.as_mut() => panic!("{veto:?}: expected the admission pause"),
        }
        assert!(turn.try_lock().is_err());
        match veto {
            Capacity => change_room(SECOND, Some(TARGET)).await,
            Readiness => ready.store(false, Ordering::SeqCst),
            Source => change_room(FIRST, Some("32")).await,
            PolicyDuringReads(read) => {
                let room = serde_json::from_value(voice_channel(TARGET, 2, "target", 0, read))
                    .expect("channel update fixture");
                let event = Event::ChannelUpdate(Box::new(room));
                pipeline.handle(&event).await.unwrap();
            }
            MemberTimeout | MemberRole | MessageRole => {
                let mut state = voice_state(FIRST, Some(SOURCE));
                let mut member = voice_member(FIRST, false);
                member["communication_disabled_until"] = member["joined_at"].clone();
                if veto == MemberRole {
                    member["roles"] = json!(["44"]);
                }
                // Remove the role through a message before publishing the matching voice frame.
                if veto == MessageRole {
                    let message = serde_json::from_value(json!({
                        "id": "50", "guild_id": staging_guild(), "channel_id": "37", "type": 0,
                        "author": member["user"], "member": member, "content": "fixture",
                        "timestamp": member["joined_at"], "tts": false, "mention_everyone": false,
                        "mentions": [], "mention_roles": [], "attachments": [], "embeds": [],
                        "pinned": false
                    }))
                    .expect("message member fixture");
                    let event = Event::MessageCreate(Box::new(message));
                    pipeline.handle(&event).await.unwrap();
                }
                state.0.member = Some(serde_json::from_value(member).expect("member fixture"));
                let event = Event::VoiceStateUpdate(Box::new(state));
                pipeline.handle(&event).await.unwrap();
            }
            Cutoff => tokio::time::sleep_until(deadline).await,
        }
        let refused = if veto == Capacity {
            let settle = async {
                lane.2.resume.notify_one();
                pending.await
            };
            let (queued, first) = tokio::join!(attempt(deadline), settle);
            assert_eq!(queued.outcome, VoiceMoveOutcome::RoomFull);
            total_reads += 6;
            first
        } else {
            lane.2.resume.notify_one();
            pending.await
        };
        assert_eq!(
            refused.outcome,
            VoiceMoveOutcome::DiscordUnavailable,
            "{veto:?}"
        );
        total_reads += expected_reads;
        mock.assert_wires(total_reads);
        ready.store(true, Ordering::SeqCst);
        change_room(FIRST, Some(SOURCE)).await;
        change_room(SECOND, Some("32")).await;
    }
    let stale = attempt(Instant::now() + Duration::from_secs(5)).await;
    assert_eq!(
        (stale.outcome, stale.from),
        (VoiceMoveOutcome::DiscordUnavailable, SOURCE.parse().ok())
    );
    assert_eq!(
        mock.count(),
        after_refresh,
        "stale refusal requires exactly one read-only refresh"
    );
    let limited = attempt(Instant::now() + Duration::from_secs(5)).await;
    assert_eq!(
        (limited.outcome, limited.retry_after_secs),
        (VoiceMoveOutcome::RateLimited, Some(1))
    );
    mock.assert_wires(second_patch + 1);
    mock.assert_patch_indices(&[first_patch, second_patch]);
    db.close().await.unwrap();
}
