#![cfg(feature = "db")]
//! Protected companion acceptance: replay fences, per-member collapse and log privacy.
//! Builders must not edit this file. Real gateway collection/draining and a
//! disposable database exercise the feature; only Discord HTTP is doubled.
//! No durable remote-delivery/outbox guarantee is claimed across an interrupted POST.

#[allow(dead_code)]
mod common;

use common::{MockRest, RestRequest, ScriptedResponse};
use serde_json::{json, Value};
use sqlx::PgPool;
use std::{
    io::Write,
    sync::{Arc, Mutex, OnceLock},
};
use twilight_model::gateway::event::Event;
use two_bot_core::{
    format_iso_millis, leveling_store as store, MemStore, OnboardingGates, OnboardingMode,
};
use two_bot_discord::{
    pipeline::MessageEligibility, ActionExecutor, LevelingRuntime, OrderedLevelingPipeline,
};
use two_bot_testsupport::TestDatabase;

const GUILD: u64 = 1545644954272137297;
const MEMBER: u64 = 100000000000000001;
const CHANNEL: u64 = 300000000000000001;
const DISPLAY_NAME: &str = "private-display-sentinel";
const MESSAGE: &str = "private-message-sentinel";
const REMOTE_ERROR: &str = "untrusted-error-sentinel";

fn at(seconds: i64) -> String {
    format_iso_millis(1_786_771_200_000 + seconds * 1000)
}

fn message(member: u64, channel: u64, seconds: i64) -> Event {
    Event::MessageCreate(Box::new(
        serde_json::from_value(json!({
            "id": (4_000_000_000_000_000_000_u64 + member + seconds as u64).to_string(),
            "guild_id": GUILD.to_string(), "channel_id": channel.to_string(),
            "author": {"id": member.to_string(), "username": DISPLAY_NAME,
                "global_name": DISPLAY_NAME, "discriminator": "0", "avatar": null, "bot": false},
            "content": MESSAGE, "timestamp": at(seconds).replace('Z', "+00:00"),
            "edited_timestamp": null, "tts": false, "mention_everyone": false,
            "mentions": [], "mention_roles": [], "attachments": [], "embeds": [],
            "pinned": false, "type": 0, "components": []
        }))
        .unwrap(),
    ))
}

fn pipeline(pool: PgPool, mock: &MockRest) -> OrderedLevelingPipeline<MemStore> {
    OrderedLevelingPipeline::new(
        MemStore::new(),
        Some(LevelingRuntime::new(
            pool,
            Arc::new(
                ActionExecutor::with_proxy("mock-only-token".into(), Some(mock.origin())).unwrap(),
            ),
            GUILD,
            OnboardingGates {
                mode: OnboardingMode::Legacy,
                dry_run: false,
            },
        )),
    )
}

async fn database() -> TestDatabase {
    let url = std::env::var("TWO_TEST_DATABASE_URL")
        .expect("ordinary DB acceptance requires the disposable test bootstrap");
    TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("create isolated migrated test database; no fallback")
}

async fn seed(pool: &PgPool, member: u64) {
    sqlx::query("INSERT INTO member_levels (guild_id,member_id,xp,imported_xp,updated_at) VALUES ($1,$2,90,90,NOW())")
        .bind(GUILD.to_string())
        .bind(member.to_string())
        .execute(pool)
        .await
        .unwrap();
}

fn receipt() -> ScriptedResponse {
    ScriptedResponse::json(200, json!({"id": "900000000000000001"}))
}

fn posts(requests: &[RestRequest]) -> Vec<&RestRequest> {
    requests
        .iter()
        .filter(|r| {
            r.method == "POST"
                && r.path.starts_with("/api/v10/channels/")
                && r.path.ends_with("/messages")
        })
        .collect()
}

fn assert_celebration(request: &RestRequest, member: u64, level: u64) {
    let body: Value = serde_json::from_slice(&request.body).unwrap();
    let content = body["content"].as_str().expect("celebration text");
    assert!(
        content.contains(&format!("<@{member}>")),
        "copy identifies the exact earning member: {content:?}"
    );
    assert!(
        content
            .split(|c: char| !c.is_ascii_alphanumeric() && c != '/' && c != '_')
            .any(|token| token == "/rank"),
        "copy points to the /rank command, not another command: {content:?}"
    );
    assert!(content.to_lowercase().contains(&format!("level {level}")));
    assert!(content.chars().count() <= 200);
    assert!(!content.contains("@everyone") && !content.contains("@here"));
    assert_eq!(body["allowed_mentions"]["parse"], json!([]));
}

// Removing the durable earning fence, or announcing a zero-XP replay, must
// fail even after both MemStore and runtime have been replaced.
#[tokio::test]
async fn duplicate_collection_drain_and_restart_never_celebrate_the_same_crossing_twice() {
    let db = database().await;
    seed(db.pool(), MEMBER).await;
    let mock = MockRest::start(vec![], receipt()).await;
    let first = pipeline(db.pool().clone(), &mock);
    let event = message(MEMBER, CHANNEL, 0);
    let stamp = at(0);
    let original = first
        .collect_at(&event, &stamp, MessageEligibility::default())
        .expect("collect");
    let already_collected = first
        .collect_at(&event, &stamp, MessageEligibility::default())
        .expect("collect");
    assert_eq!(original.len(), 1);
    let awards = first.drain(original).await.unwrap();
    assert_eq!(awards.len(), 1);
    assert_eq!(
        (awards[0].awarded, awards[0].total_xp, awards[0].level),
        (15, 105, 1)
    );
    assert!(awards[0].leveled_up);
    let requests = mock.requests();
    assert_eq!(
        requests.len(),
        1,
        "no role ladder: only the initial celebration"
    );
    assert_eq!(posts(&requests).len(), 1);
    assert_eq!(
        requests[0].path,
        format!("/api/v10/channels/{CHANNEL}/messages")
    );
    assert_celebration(&requests[0], MEMBER, 1);

    let awards = first.drain(already_collected).await.unwrap();
    assert!(awards.iter().all(|a| a.awarded == 0 && !a.leveled_up));
    assert_eq!(
        mock.requests().len(),
        1,
        "a second pre-collected batch cannot re-post"
    );
    let repeated = first
        .collect_at(&event, &stamp, MessageEligibility::default())
        .expect("collect");
    let awards = first.drain(repeated).await.unwrap();
    assert!(awards.iter().all(|a| a.awarded == 0 && !a.leveled_up));
    assert!(first.drain(vec![]).await.unwrap().is_empty());
    assert_eq!(
        mock.requests().len(),
        1,
        "recollection and an empty drain stay silent"
    );
    drop(first);

    let peer = db
        .independent_pool()
        .await
        .expect("new connection after restart");
    let restarted = pipeline(peer.clone(), &mock);
    let awards = restarted
        .handle_at(&event, &stamp, MessageEligibility::default())
        .await
        .unwrap();
    assert_eq!(awards.len(), 1);
    assert_eq!(awards[0].awarded, 0);
    assert!(!awards[0].leveled_up);
    assert_eq!(
        mock.requests().len(),
        1,
        "durable replay is silent with fresh runtime state"
    );
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM xp_awards")
        .fetch_one(&peer)
        .await
        .unwrap();
    assert_eq!(rows, 1, "replays never append another earning row");

    // The replay fence is not a permanent member mute: a fresh eligible
    // message still earns normally, without inventing another crossing.
    let awards = restarted
        .handle_at(
            &message(MEMBER, CHANNEL, 60),
            &at(60),
            MessageEligibility::default(),
        )
        .await
        .unwrap();
    assert_eq!((awards[0].awarded, awards[0].total_xp), (15, 120));
    assert!(!awards[0].leveled_up);
    assert_eq!(mock.requests().len(), 1);
    let profile = store::profile(&peer, &GUILD.to_string(), &MEMBER.to_string())
        .await
        .unwrap();
    assert_eq!((profile.xp, profile.message_xp), (120, 30));
    drop(restarted);
    peer.close().await;
    mock.shutdown().await;
    db.close().await.expect("drop test database");
}

// A global collapse would lose one member; no collapse would post four times.
// Each member crosses floors 100 and 255 in the same interleaved drain.
#[tokio::test]
async fn one_drain_collapses_multiple_crossings_per_member_not_globally() {
    let db = database().await;
    for member in [MEMBER, MEMBER + 1] {
        seed(db.pool(), member).await;
    }
    let mock = MockRest::start(vec![], receipt()).await;
    let pipeline = pipeline(db.pool().clone(), &mock);
    let stamps: Vec<i64> = (0..10)
        .map(|i| i * 120)
        // Index 14 crosses level 2; index 15 is an eligible non-crossing
        // tail in another channel. Neither first nor last channel is right.
        .chain((0..6).map(|i| 68_000 + i * 120))
        .collect();
    let mut batch = Vec::new();
    for (index, seconds) in stamps.iter().enumerate() {
        for offset in [0_u64, 1] {
            // Disjoint channel sets pair each exact crossing channel with
            // its own member, independent of post order or incidental prose.
            batch.extend(
                pipeline
                    .collect_at(
                        &message(
                            MEMBER + offset,
                            CHANNEL + offset * 100 + index as u64,
                            *seconds,
                        ),
                        &at(*seconds),
                        MessageEligibility::default(),
                    )
                    .expect("collect"),
            );
        }
    }
    assert_eq!(batch.len(), 32);
    let awards = pipeline.drain(batch).await.unwrap();
    assert_eq!(awards.len(), 32);
    assert_eq!(awards.iter().filter(|a| a.leveled_up).count(), 4);
    let expected = [15, 15, 15, 15, 15, 5, 5, 5, 5, 5, 15, 15, 15, 15, 15, 5];
    for offset in [0_usize, 1] {
        assert_eq!(
            awards
                .iter()
                .skip(offset)
                .step_by(2)
                .map(|a| a.awarded)
                .collect::<Vec<_>>(),
            expected
        );
        let profile = store::profile(
            db.pool(),
            &GUILD.to_string(),
            &(MEMBER + offset as u64).to_string(),
        )
        .await
        .unwrap();
        assert_eq!(
            (profile.xp, profile.message_xp, profile.level),
            (270, 180, 2)
        );
    }
    let requests = mock.requests();
    let celebrations = posts(&requests);
    assert_eq!(
        requests.len(),
        2,
        "only the two per-member celebrations reach Discord"
    );
    assert_eq!(celebrations.len(), 2);
    for offset in [0_u64, 1] {
        let owned: Vec<_> = celebrations
            .iter()
            .filter(|r| {
                r.path == format!("/api/v10/channels/{}/messages", CHANNEL + offset * 100 + 14)
            })
            .collect();
        assert_eq!(
            owned.len(),
            1,
            "each member owns one post in the highest crossing's exact channel"
        );
        assert_celebration(owned[0], MEMBER + offset, 2);
        assert!(awards[28 + offset as usize].leveled_up);
        assert_eq!(awards[28 + offset as usize].total_xp, 265);
        assert!(!awards[30 + offset as usize].leveled_up);
        assert_eq!(awards[30 + offset as usize].total_xp, 270);
    }
    assert!(pipeline.drain(vec![]).await.unwrap().is_empty());
    assert_eq!(
        mock.requests().len(),
        2,
        "a subsequent drain cannot flush stale crossings"
    );
    mock.shutdown().await;
    db.close().await.expect("drop test database");
}

// Process-global capture avoids async/thread-local callsite-interest races.
// Byte marks bound each window; only this test produces failed feature POSTs.
#[derive(Clone, Default)]
struct LogSink(Arc<Mutex<Vec<u8>>>);
impl Write for LogSink {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn log_sink() -> LogSink {
    static SINK: OnceLock<LogSink> = OnceLock::new();
    SINK.get_or_init(|| {
        let sink = LogSink::default();
        let writer = sink.clone();
        tracing::subscriber::set_global_default(
            tracing_subscriber::fmt()
                .json()
                .with_max_level(tracing::Level::WARN)
                .with_writer(move || writer.clone())
                .finish(),
        )
        .expect("install process-wide test capture once");
        sink
    })
    .clone()
}
fn feature_events_since(sink: &LogSink, mark: usize) -> Vec<Value> {
    let bytes = sink.0.lock().unwrap()[mark..].to_vec();
    String::from_utf8(bytes)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("structured tracing event"))
        // Never mistake a Hyper/Twilight/SQLx error for our feature event.
        .filter(|event| event["target"] == "two_bot_discord::leveling_runtime")
        .collect()
}

// Logging a raw remote error or adding even one caller-controlled field must
// fail: the ENTIRE feature field map, not only its reason, is allowlisted.
#[tokio::test]
async fn failed_post_logs_only_fixed_vocabulary_without_member_or_remote_data() {
    let sink = log_sink();
    let db = database().await;
    for offset in 0..4 {
        seed(db.pool(), MEMBER + offset).await;
    }
    let poison = format!("{REMOTE_ERROR} {DISPLAY_NAME} {MESSAGE} {GUILD} {MEMBER} {CHANNEL}");
    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(403, json!({"code":50013, "message":poison})),
            ScriptedResponse::json(500, json!({"code":0, "message":poison})),
            // HTTP success without a valid receipt is also a failed send.
            ScriptedResponse::json(200, json!({"untrusted":poison})),
            receipt(),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let pipeline = pipeline(db.pool().clone(), &mock);
    for offset in 0..4 {
        let mark = sink.0.lock().unwrap().len();
        let awards = pipeline
            .handle_at(
                &message(MEMBER + offset, CHANNEL + offset, 0),
                &at(0),
                MessageEligibility::default(),
            )
            .await
            .expect("POST failure never fails the durable award");
        assert_eq!(awards.len(), 1);
        assert_eq!((awards[0].awarded, awards[0].total_xp), (15, 105));
        assert!(awards[0].leveled_up);
        let events = feature_events_since(&sink, mark);
        if offset == 3 {
            assert!(
                events.is_empty(),
                "a successful POST emits no feature failure"
            );
        } else {
            assert_eq!(
                events.len(),
                1,
                "exactly one feature failure event, not dependency output: {events:?}"
            );
            assert_eq!(events[0]["level"], "WARN");
            let cause = ["rejected", "unavailable", "unavailable"][offset as usize];
            assert_eq!(
                events[0]["fields"],
                json!({
                    "message": "level-up announcement delivery failed", "cause": cause
                }),
                "all feature fields must have fixed, member-data-free values"
            );
            let fields = events[0].to_string();
            for forbidden in [
                REMOTE_ERROR.to_owned(),
                DISPLAY_NAME.to_owned(),
                MESSAGE.to_owned(),
                GUILD.to_string(),
                (MEMBER + offset).to_string(),
                (CHANNEL + offset).to_string(),
            ] {
                assert!(
                    !fields.contains(&forbidden),
                    "feature event leaks fixture data"
                );
            }
        }
        let profile = store::profile(
            db.pool(),
            &GUILD.to_string(),
            &(MEMBER + offset).to_string(),
        )
        .await
        .unwrap();
        assert_eq!((profile.xp, profile.message_xp), (105, 15));
    }
    assert_eq!(
        mock.requests().len(),
        4,
        "one bounded POST per crossing, no retries"
    );
    let requests = mock.requests();
    let celebrations = posts(&requests);
    assert_eq!(celebrations.len(), 4);
    for (offset, request) in celebrations.iter().enumerate() {
        assert_eq!(
            request.path,
            format!("/api/v10/channels/{}/messages", CHANNEL + offset as u64)
        );
        assert_celebration(request, MEMBER + offset as u64, 1);
    }
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM xp_awards")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(rows, 4);
    mock.shutdown().await;
    db.close().await.expect("drop test database");
}
