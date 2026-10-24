//! Protected worker acceptance. Runs apply_dispatch, not its error helpers.
//! Only the guarded disposable database and a held loopback REST tripwire are used.
#![cfg(test)]

use super::*;
use std::sync::atomic::AtomicBool;
use two_bot_core::{gateway_funnel::FunnelBatch, ClassifierConfig, FactsSink, RulesAcceptedFact};
use two_bot_discord::leveling_runtime::{deferred_worker_poison as fault, DeferredCommunityFacts};

const AT: &str = "2026-10-20T12:00:00.000Z";
const ACTION: &str = "supervisor_restart_inspection";

#[derive(Clone, Copy)]
enum Boundary {
    CollectLeveling,
    CollectFacts,
    DrainFacts,
}

fn message() -> Event {
    Event::MessageCreate(Box::new(
        serde_json::from_value(serde_json::json!({
            "id":"4000000000000000001", "guild_id":"22", "channel_id":"66",
            "author":{"id":"44","username":"fixture","discriminator":"0","avatar":null,"bot":false},
            "content":"synthetic-member-content", "timestamp":AT.replace('Z', "+00:00"),
            "edited_timestamp":null, "tts":false,"mention_everyone":false,
            "mentions":[],"mention_roles":[],"attachments":[],"embeds":[],
            "pinned":false,"type":0,"components":[]
        }))
        .expect("complete synthetic message"),
    ))
}

fn checkpoint(sequence: u64) -> GatewaySession {
    GatewaySession {
        session_id: "fixture-session".into(),
        sequence,
        resume_url: "wss://gateway.invalid".into(),
        updated_at_ms: 1_792_497_600_000,
    }
}

// Voice is already called between successful collection and the facts drain.
// Inject in that existing synchronous boundary, never via a production hook,
// process-global latch, detached thread or timing sleep.
struct PoisonAfterCollect {
    facts: DeferredCommunityFacts,
    called: AtomicBool,
}
impl VoiceEventSink for PoisonAfterCollect {
    fn handle(&self, event: &Event, _: &twilight_cache_inmemory::DefaultInMemoryCache) {
        assert!(matches!(event, Event::Resumed));
        assert!(
            !self.called.swap(true, Ordering::AcqRel),
            "one dispatch only"
        );
        assert_eq!(
            self.facts.len().expect("facts healthy through collection"),
            1
        );
        fault::poison_facts(&self.facts);
    }
    fn presence(&self, _: &twilight_model::gateway::payload::incoming::PresenceUpdate) {
        panic!("fixture is not a presence dispatch");
    }
    fn disconnect(&self) {
        panic!("fixture generation did not change");
    }
    fn needs_bootstrap(&self, _: &twilight_cache_inmemory::DefaultInMemoryCache) -> bool {
        false
    }
}

async fn exercise(boundary: Boundary, inject: bool) {
    let url = std::env::var("TWO_TEST_DATABASE_URL")
        .expect("worker acceptance requires the guarded disposable test bootstrap");
    let db =
        two_bot_testsupport::TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("create isolated migrated agent-testdb fixture");
    let pool = db.pool().clone();
    let store = GatewaySessionStore::new(pool.clone(), "22".into(), 1);
    let baseline = checkpoint(6);
    store
        .commit_dispatch(&baseline, FunnelBatch::default())
        .await
        .expect("seed cursor");
    // Observe every checkpoint write, including an accidental same-value
    // UPDATE. Cursor equality alone would miss such an attempted commit.
    sqlx::raw_sql(
        "CREATE TABLE acceptance_checkpoint_writes (n integer NOT NULL);
         CREATE FUNCTION acceptance_watch_checkpoint_write() RETURNS trigger
         LANGUAGE plpgsql AS $$ BEGIN
             INSERT INTO acceptance_checkpoint_writes VALUES (1);
             RETURN NEW;
         END $$;
         CREATE TRIGGER acceptance_checkpoint_write
         AFTER INSERT OR UPDATE ON gateway_sessions FOR EACH ROW
         EXECUTE FUNCTION acceptance_watch_checkpoint_write();",
    )
    .execute(&pool)
    .await
    .expect("install fixture-only checkpoint witness");

    let rest = std::net::TcpListener::bind("127.0.0.1:0").expect("hold REST tripwire");
    rest.set_nonblocking(true)
        .expect("nonblocking REST observation");
    let _ = rustls::crypto::ring::default_provider().install_default();
    let executor = two_bot_discord::ActionExecutor::with_proxy(
        "mock-only-token".into(),
        Some(format!(
            "http://{}",
            rest.local_addr().expect("REST address")
        )),
    )
    .expect("loopback-only executor");
    let pipeline = build_pipeline(
        vec![],
        Some(LevelingRuntime::new(
            pool.clone(),
            Arc::new(executor),
            22,
            two_bot_core::OnboardingGates {
                mode: two_bot_core::OnboardingMode::Legacy,
                dry_run: true,
            },
        )),
    );
    pipeline.set_message_content_delivered(true);
    let config = ClassifierConfig::from_map(
        &[
            (
                "TWO_COMMUNITY_CLASSIFIER_VERSION".into(),
                "worker-fixture-v1".into(),
            ),
            ("TWO_COMMUNITY_HUMAN_CHANNEL_IDS".into(), "66".into()),
        ]
        .into_iter()
        .collect(),
    );
    pipeline
        .refresh_classifier_config(config.clone())
        .expect("healthy paired classifier");
    let facts = pipeline
        .handlers()
        .facts()
        .expect("real deferred sink")
        .clone();
    // Leveling poison must not suppress an independent healthy facts writer.
    // Keep capture disabled in that fixture rather than asserting the wrong
    // cross-buffer contract. The other fixtures exercise the armed sink.
    if !matches!(boundary, Boundary::CollectLeveling) {
        facts
            .enable_with_config(pool.clone(), config)
            .expect("arm real facts sink");
    }
    if matches!(boundary, Boundary::DrainFacts) {
        facts.record_rules_accepted(RulesAcceptedFact {
            guild_id: 22,
            member_id: 44,
            is_bot: false,
            occurred_at: AT,
            source_event_id: "22:44:fixture-rules".into(),
            source: "gateway",
        });
        assert!(pipeline
            .collect_at(
                &Event::Resumed,
                AT,
                two_bot_discord::MessageEligibility::default()
            )
            .expect("healthy collection precondition")
            .is_empty());
        assert_eq!(facts.len().expect("one real queued fact"), 1);
    }
    let late = Arc::new(PoisonAfterCollect {
        facts: facts.clone(),
        called: AtomicBool::new(false),
    });
    if inject {
        match boundary {
            Boundary::CollectLeveling => fault::poison_leveling(&pipeline),
            Boundary::CollectFacts => fault::poison_facts(&facts),
            Boundary::DrainFacts => {}
        }
    }
    let voice: Option<Arc<dyn VoiceEventSink>> =
        if inject && matches!(boundary, Boundary::DrainFacts) {
            Some(late.clone())
        } else {
            None
        };
    let event = if matches!(boundary, Boundary::DrainFacts) {
        Event::Resumed
    } else {
        message()
    };
    let mut dispatch = ReceivedDispatch::new(event);
    dispatch.observed_at = AT.into();
    let state = RwLock::new(GatewayState::Armed);
    let generation = AtomicU64::new(0);
    let failures = AtomicU64::new(0);
    let outcome = tokio::task::block_in_place(|| {
        apply_dispatch(
            &tokio::runtime::Handle::current(),
            &state,
            &generation,
            &failures,
            &pipeline,
            &store,
            None,
            None,
            voice,
            None,
            LiveInteractions::default(),
            Arc::new(tokio::sync::Notify::new()),
            Some(Box::new(dispatch)),
            None,
            checkpoint(7),
            CHECKPOINT_IO_MAX,
            0,
            None,
            None,
        )
    });
    let saved = store
        .load()
        .await
        .expect("read durable cursor")
        .expect("baseline session exists");
    let writes: i64 = sqlx::query_scalar("SELECT count(*) FROM acceptance_checkpoint_writes")
        .fetch_one(&pool)
        .await
        .expect("count actual checkpoint writes");
    let awards: i64 = sqlx::query_scalar("SELECT count(*) FROM xp_awards")
        .fetch_one(&pool)
        .await
        .expect("count real award writes");
    let captures: i64 = sqlx::query_scalar("SELECT count(*) FROM community_facts")
        .fetch_one(&pool)
        .await
        .expect("count real captured facts");
    let funnel: i64 = sqlx::query_scalar("SELECT count(*) FROM events")
        .fetch_one(&pool)
        .await
        .expect("count committed funnel effects");
    let unchanged_state = *state.read().await == GatewayState::Armed;
    db.close()
        .await
        .expect("verify disposable fixture teardown");
    assert!(
        matches!(rest.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
        "no Discord reward/capture work is fabricated"
    );

    if inject {
        assert!(
            saved == baseline,
            "failed dispatch must preserve the entire durable cursor"
        );
        assert_eq!(
            writes, 0,
            "poison is pre_commit, never a failed checkpoint write"
        );
        assert_eq!(
            (awards, captures, funnel),
            (0, 0, 0),
            "no fabricated award, capture or committed funnel work"
        );
        assert!(
            unchanged_state,
            "poison must not publish connected checkpoint success"
        );
        assert!(
            matches!(&outcome, Err(sqlx::Error::InvalidArgument(_))),
            "typed worker refusal, got {outcome:?}"
        );
        let terminal =
            crate::db_log::failure_reason(outcome.as_ref().expect_err("poison cannot succeed"));
        let reason = if matches!(boundary, Boundary::CollectLeveling) {
            "deferred_leveling_poisoned"
        } else {
            "deferred_community_facts_poisoned"
        };
        assert!(
            terminal.contains(reason),
            "final supervisor reason lost {reason}: {terminal}"
        );
        assert!(terminal.contains(ACTION));
        assert!(terminal.contains("checkpoint unchanged"));
        assert!(terminal.len() <= 256);
        for forbidden in [
            "synthetic-worker-poison",
            "synthetic-member-content",
            "PoisonError",
            "postgres",
            "sqlx",
        ] {
            assert!(
                !terminal.contains(forbidden),
                "unsafe supervisor diagnostic"
            );
        }
        match boundary {
            Boundary::CollectLeveling => fault::assert_leveling_preserved(&pipeline, 0),
            Boundary::CollectFacts => fault::assert_facts_preserved(&facts, (0, 0, 0)),
            Boundary::DrainFacts => {
                assert!(
                    late.called.load(Ordering::Acquire),
                    "fault must occur after collection inside worker"
                );
                fault::assert_facts_preserved(&facts, (0, 1, 0));
            }
        }
    } else {
        assert!(
            outcome.is_ok(),
            "independent healthy worker must still commit: {outcome:?}"
        );
        assert!(
            saved == checkpoint(7),
            "healthy cursor must advance normally"
        );
        assert_eq!(writes, 1, "witness must detect a real healthy commit");
        match boundary {
            Boundary::CollectLeveling => assert_eq!((awards, captures), (1, 0)),
            Boundary::CollectFacts => assert_eq!((awards, captures), (1, 1)),
            Boundary::DrainFacts => assert_eq!((awards, captures), (0, 1)),
        }
        assert!(!late.called.load(Ordering::Acquire));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn collection_leveling_poison_is_pre_commit_with_final_supervisor_reason() {
    exercise(Boundary::CollectLeveling, true).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn collection_facts_poison_is_pre_commit_with_final_supervisor_reason() {
    exercise(Boundary::CollectFacts, true).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn facts_poison_first_discovered_at_worker_drain_holds_cursor() {
    exercise(Boundary::DrainFacts, true).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn healthy_leveling_worker_still_awards_and_commits() {
    exercise(Boundary::CollectLeveling, false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn healthy_facts_worker_still_captures_awards_and_commits() {
    exercise(Boundary::CollectFacts, false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn healthy_facts_drain_still_captures_and_commits() {
    exercise(Boundary::DrainFacts, false).await;
}
