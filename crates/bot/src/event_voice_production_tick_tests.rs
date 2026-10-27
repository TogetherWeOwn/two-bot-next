//! Builder-owned production tick composition: preflight, reconcile, fresh
//! source, durable decision and binding persistence through one Context.
use super::*;
use crate::discord_test_common::{MockRest, ScriptedResponse};
use serde_json::json;
use time::OffsetDateTime;
use twilight_cache_inmemory::DefaultInMemoryCache;
use twilight_gateway::Event;
use two_bot::voice_rooms::{LiveGuild, VoiceEventSink};
use two_bot_core::voice_create_admission::CreateAdmissionConfig;
use two_bot_cutover::event_voice::{
    bind_admitted_event_voice_channel, claim_current_event_voice_channel, EventVoiceCreateBinding,
};
use two_bot_cutover::voice_rooms::{CreateClaim, PgRoomStore};
use two_bot_testsupport::TestDatabase;

struct Sink(LiveGuild);
impl VoiceEventSink for Sink {
    fn handle(&self, _: &Event, _: &DefaultInMemoryCache) {}
    fn presence(&self, _: &twilight_model::gateway::payload::incoming::PresenceUpdate) {}
    fn disconnect(&self) {}
    fn needs_bootstrap(&self, _: &DefaultInMemoryCache) -> bool {
        false
    }
    fn event_voice_live(&self, guild: u64) -> Option<LiveGuild> {
        (guild == 100).then(|| self.0.clone())
    }
}

async fn database() -> Option<TestDatabase> {
    let url = std::env::var("TWO_TEST_DATABASE_URL").ok()?;
    Some(
        TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .unwrap(),
    )
}

/// Event 501 bound to 701; the tick's reconcile GET 404s and retires it.
async fn seed_orphan_candidate(pool: &PgPool) {
    let at = (OffsetDateTime::now_utc() + time::Duration::minutes(10))
        .replace_nanosecond(0)
        .unwrap();
    claim_current_event_voice_channel(pool, "501", at, at, None, "seed-lease")
        .await
        .unwrap()
        .unwrap();
    let CreateClaim::Admitted { reservation_id } = PgRoomStore::new(pool.clone())
        .claim_create(
            100,
            999,
            &CreateAdmissionConfig::new(10, 10, 0).unwrap(),
            at.unix_timestamp(),
        )
        .await
        .unwrap()
    else {
        panic!("fixture admission");
    };
    assert!(bind_admitted_event_voice_channel(
        pool,
        &EventVoiceCreateBinding {
            guild_id: 100,
            event_id: "501",
            instance_starts_at: at,
            lease: "seed-lease",
            reservation_id: &reservation_id,
            channel_id: "701",
        },
    )
    .await
    .unwrap());
}

fn source_document() -> Value {
    let starts = two_bot_core::funnel::format_iso_millis(
        i64::try_from(
            (OffsetDateTime::now_utc() + time::Duration::minutes(10)).unix_timestamp_nanos()
                / 1_000_000,
        )
        .unwrap(),
    );
    json!({"id":"500","guild_id":"100","name":"Fresh event","entity_type":2,
        "status":1,"scheduled_start_time":starts,
        "scheduled_end_time":null,"channel_id":null})
}

#[tokio::test]
async fn production_tick_reconciles_then_creates_and_binds_through_the_guarded_path() {
    let Some(db) = database().await else { return };
    seed_orphan_candidate(db.pool()).await;
    let mock = MockRest::start(
        vec![
            // Preflight category GET.
            ScriptedResponse::json(
                200,
                json!({"id":"200","guild_id":"100","type":4,
                    "name":"events","permission_overwrites":[]}),
            ),
            // Reconcile GET on the dead binding.
            ScriptedResponse::status(404),
            // Fresh source read.
            ScriptedResponse::json(200, json!([source_document()])),
            // Bot identity for capacity admission.
            ScriptedResponse::json(200, json!({"id":"999","bot":true})),
            // Durable create POST.
            ScriptedResponse::json(
                201,
                json!({"id":"700","guild_id":"100","parent_id":"200","type":2}),
            ),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    crate::gateway::ensure_crypto_provider();
    let context = Context {
        url: "invalid fixture database; must never connect".into(),
        pool: OnceCell::new(),
        rest: ActionExecutor::with_proxy(
            "synthetic-production-tick-token".into(),
            Some(mock.origin()),
        )
        .unwrap(),
        guild: "100".into(),
        observation: Mutex::new(()),
        events_gate: JobGate::default(),
        event_voice: event_voice_preflight::Tick::default(),
        event_voice_source: event_voice_source::Reader::default(),
        event_voice_reconcile: event_voice_reconcile::Reconciler::default(),
        voice: Some(Arc::new(Sink(super::event_voice_preflight_tests::live()))),
    };
    context.pool.set(db.pool().clone()).unwrap();
    let (_stop, shutdown) = watch::channel(false);
    context
        .run_event_voice_tick_guarded(
            event_voice_preflight::Settings {
                category: Some(200),
                staff_channel: None,
            },
            CreateAdmissionConfig::new(10, 10, 0).unwrap(),
            &shutdown,
        )
        .await
        .unwrap();
    let requests = mock.requests();
    assert_eq!(
        requests.len(),
        5,
        "preflight + reconcile + source + actor + create, nothing else"
    );
    assert_eq!(requests[0].method, "GET");
    assert!(requests[0].path.ends_with("/channels/200"));
    assert_eq!(requests[1].method, "GET");
    assert!(requests[1].path.ends_with("/channels/701"));
    assert!(requests[2].path.contains("scheduled-events"));
    assert!(requests[3].path.ends_with("/users/@me"));
    assert_eq!(requests[4].method, "POST");
    assert!(requests[4].path.ends_with("/guilds/100/channels"));
    let created: Option<String> = sqlx::query_scalar(
        "SELECT voice_channel_id FROM event_voice_channels WHERE event_id='500'",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(created, Some("700".to_owned()));
    let retired: (String, bool) = sqlx::query_as(
        "SELECT e.status, r.settled_at IS NOT NULL FROM event_voice_channels e
         JOIN voice_create_reservations r ON r.channel_id = e.voice_channel_id
         WHERE e.event_id='501'",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(retired, ("orphaned".to_owned(), true));
    mock.shutdown().await;
    db.close().await.unwrap();
}
