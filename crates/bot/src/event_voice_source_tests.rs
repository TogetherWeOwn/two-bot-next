//! Builder-owned source/binding integration; protected acceptance files stay unchanged.
use super::*;
use crate::discord_test_common::{MockRest, ScriptedResponse};
use serde_json::json;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};
use two_bot_core::settings::{WriteAction, EVENT_VOICE_OPT_OUT_KEY};
use two_bot_cutover::{event_voice::claim_event_voice_channel, settings::SettingsStore};
use two_bot_discord::voice_rooms::RoomHttpError;
use two_bot_testsupport::TestDatabase;

fn document(id: &str, kind: i64) -> Value {
    json!({"id":id,"guild_id":"100","name":"Voice event",
        "scheduled_start_time":"2026-10-23T22:10:00.123Z",
        "scheduled_end_time":null,"channel_id":null,"entity_type":kind,"status":1})
}

async fn database() -> Option<TestDatabase> {
    let url = std::env::var("TWO_TEST_DATABASE_URL").ok()?;
    Some(
        TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .unwrap(),
    )
}

fn executor(mock: &MockRest) -> ActionExecutor {
    crate::gateway::ensure_crypto_provider();
    ActionExecutor::with_proxy("synthetic-event-source-token".into(), Some(mock.origin())).unwrap()
}

#[test]
fn source_maps_only_explicit_event_types_and_preserves_stage_attachment() {
    let mut stage = document("501", 1);
    stage["channel_id"] = json!("700");
    let mut voice = document("500", 2);
    voice["scheduled_end_time"] = json!("2026-10-23T23:10:00.123Z");
    let external = document("502", 3);
    let rows = event_voice_source::parse(&json!([voice, stage, external]), "100").unwrap();
    assert_eq!(rows[0]["entity_type"], "VOICE");
    assert_eq!(rows[0]["status"], "SCHEDULED");
    assert_eq!(rows[0]["instance_starts_at_ms"], 1_792_793_400_123_i64);
    assert_eq!(rows[0]["ends_at_ms"], 1_792_797_000_123_i64);
    assert_eq!(rows[1]["entity_type"], "STAGE");
    assert_eq!(rows[1]["attached_channel_id"], "700");
    assert_eq!(rows[2]["entity_type"], "EXTERNAL");
    assert!(rows[2]["ends_at_ms"].is_null());
    for (status, expected) in [(2, "ACTIVE"), (3, "COMPLETED"), (4, "CANCELED")] {
        let mut value = document("500", 2);
        value["status"] = json!(status);
        assert_eq!(
            event_voice_source::parse(&json!([value]), "100").unwrap()[0]["status"],
            expected
        );
    }
}

#[test]
fn malformed_or_ambiguous_source_refuses_whole_snapshot_without_inferring_voice() {
    let valid = document("500", 2);
    for (key, bad) in [
        ("guild_id", json!("101")),
        ("id", json!("0500")),
        ("entity_type", json!(null)),
        ("entity_type", json!(4)),
        ("entity_type", json!("2")),
        ("status", json!(0)),
        ("scheduled_start_time", json!("tomorrow")),
        ("scheduled_end_time", json!("2026-10-23T21:10:00Z")),
        ("channel_id", json!("0")),
        ("name", json!(" ")),
        ("name", json!("a".repeat(101))),
    ] {
        let mut invalid = valid.clone();
        invalid[key] = bad;
        assert!(
            event_voice_source::parse(&json!([valid.clone(), invalid]), "100").is_err(),
            "{key}"
        );
    }
    assert!(event_voice_source::parse(&json!([valid.clone(), valid.clone()]), "100").is_err());
    assert!(event_voice_source::parse(&json!(vec![valid; 101]), "100").is_err());
    assert!(event_voice_source::parse(&json!({}), "100").is_err());
    assert!(event_voice_source::parse(&json!([]), "0100").is_err());
    assert!(event_voice_source::parse(&json!([]), "100")
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn fresh_event_source_reads_once_and_retains_route_retry_after() {
    for status in [403, 404, 429, 500] {
        let mock = MockRest::start(
            vec![ScriptedResponse::json(
                status,
                json!({"retry_after":180.25}),
            )],
            ScriptedResponse::json(200, json!([])),
        )
        .await;
        let result = executor(&mock).read_event_voice_events(100).await;
        match status {
            403 => assert_eq!(result.unwrap_err(), RoomHttpError::AccessDenied),
            404 => assert_eq!(result.unwrap_err(), RoomHttpError::NotFound),
            429 => assert_eq!(
                result.unwrap_err(),
                RoomHttpError::RateLimited {
                    retry_after_ms: 180_250,
                    global: false
                }
            ),
            _ => assert_eq!(result.unwrap_err(), RoomHttpError::UnknownOutcome),
        }
        let requests = mock.requests();
        assert_eq!(requests.len(), 1, "no hidden HTTP retry");
        assert_eq!(requests[0].path, "/api/v10/guilds/100/scheduled-events");
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn fresh_event_source_enforces_wire_size_and_canonical_guild_before_io() {
    let body = format!("[{}]{}", document("500", 2), " ".repeat(1_048_576));
    let mock = MockRest::start(
        vec![],
        ScriptedResponse {
            body: body.into_bytes(),
            ..ScriptedResponse::status(200)
        },
    )
    .await;
    let rest = executor(&mock);
    assert_eq!(
        rest.read_event_voice_events(0).await.unwrap_err(),
        RoomHttpError::InvalidRequest
    );
    assert!(mock.requests().is_empty());
    assert_eq!(
        rest.read_event_voice_events(100).await.unwrap_err(),
        RoomHttpError::UnknownOutcome
    );
    assert_eq!(mock.requests().len(), 1);
    mock.shutdown().await;
}

#[tokio::test]
async fn source_keeps_claim_identity_and_schedule_on_small_and_far_future_moves() {
    let Some(db) = database().await else { return };
    let original = OffsetDateTime::parse("2026-10-23T22:10:00.123Z", &Rfc3339).unwrap();
    assert!(claim_event_voice_channel(
        db.pool(),
        "500",
        original,
        original,
        None,
        "held-before-restart"
    )
    .await
    .unwrap());
    sqlx::query("UPDATE event_voice_channels SET voice_channel_id='700' WHERE event_id='500'")
        .execute(db.pool())
        .await
        .unwrap();
    for new_start in ["2026-10-23T22:15:00.123Z", "2026-10-26T22:15:00.123Z"] {
        let mut event = document("500", 2);
        event["scheduled_start_time"] = json!(new_start);
        let rows = event_voice_source::load(db.pool(), "100", &json!([event]))
            .await
            .unwrap();
        assert_eq!(rows[0]["binding_key_ms"], 1_792_793_400_123_i64);
        assert_eq!(rows[0]["voice_channel_id"], "700");
        assert_eq!(
            rows[0]["occupancy_known"], false,
            "source reads cannot authorize deletion"
        );
        assert_eq!(
            rows[0]["binding_deleted"], false,
            "only live 404 may authorize recreation"
        );
        let stored:(OffsetDateTime,OffsetDateTime,Option<String>)=sqlx::query_as("SELECT instance_starts_at,starts_at,owner_tick_lease FROM event_voice_channels WHERE event_id='500'").fetch_one(db.pool()).await.unwrap();
        assert_eq!(stored.0, original);
        assert_eq!(
            stored.1,
            OffsetDateTime::parse(new_start, &Rfc3339).unwrap()
        );
        assert_eq!(stored.2.as_deref(), Some("held-before-restart"));
    }
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM event_voice_channels")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(count, 1);
    db.close().await.unwrap();
}

#[tokio::test]
async fn source_preserves_opt_out_independently_of_mirror_replacement_and_guild() {
    let Some(db) = database().await else { return };
    let store = SettingsStore::new(db.pool());
    let write = store
        .set(
            "100",
            EVENT_VOICE_OPT_OUT_KEY,
            Some(json!(["500"])),
            "moderator",
        )
        .await
        .unwrap();
    assert_eq!(write.action, WriteAction::Upsert(json!(["500"])));
    store
        .set(
            "101",
            EVENT_VOICE_OPT_OUT_KEY,
            Some(json!(["501"])),
            "other-moderator",
        )
        .await
        .unwrap();
    let response = json!([document("500", 2), document("501", 2)]);
    for _ in 0..2 {
        two_bot_core::replace_events(db.pool(), "100", &now_iso(), &[])
            .await
            .unwrap();
        let rows = event_voice_source::load(db.pool(), "100", &response)
            .await
            .unwrap();
        assert_eq!(rows[0]["voice_opt_out"], true);
        assert_eq!(rows[1]["voice_opt_out"], false);
    }
    assert!(event_voice_source::opted_out(db.pool(), "100", "500")
        .await
        .unwrap());
    assert!(!event_voice_source::opted_out(db.pool(), "101", "500")
        .await
        .unwrap());
    store
        .set("100", EVENT_VOICE_OPT_OUT_KEY, None, "moderator")
        .await
        .unwrap();
    assert!(!event_voice_source::opted_out(db.pool(), "100", "500")
        .await
        .unwrap());
    let audit: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM guild_settings_audit WHERE guild_id='100' AND key=$1",
    )
    .bind(EVENT_VOICE_OPT_OUT_KEY)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(audit, 2);
    db.close().await.unwrap();
}

#[tokio::test]
async fn malformed_stored_opt_out_fails_closed_before_refreshing_bindings() {
    let Some(db) = database().await else { return };
    let original = OffsetDateTime::parse("2026-10-23T22:10:00.123Z", &Rfc3339).unwrap();
    claim_event_voice_channel(db.pool(), "500", original, original, None, "held")
        .await
        .unwrap();
    SettingsStore::new(db.pool())
        .set(
            "100",
            EVENT_VOICE_OPT_OUT_KEY,
            Some(json!(["0500"])),
            "moderator",
        )
        .await
        .unwrap();
    let mut event = document("500", 2);
    event["scheduled_start_time"] = json!("2026-10-26T22:10:00.123Z");
    assert_eq!(
        event_voice_source::load(db.pool(), "100", &json!([event]))
            .await
            .unwrap_err(),
        ErrorClass::Configuration
    );
    assert_eq!(
        event_voice_source::opted_out(db.pool(), "100", "500")
            .await
            .unwrap_err(),
        ErrorClass::Configuration
    );
    let starts: OffsetDateTime =
        sqlx::query_scalar("SELECT starts_at FROM event_voice_channels WHERE event_id='500'")
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(starts, original);
    db.close().await.unwrap();
}

#[tokio::test]
async fn durable_opt_out_overrides_a_queued_create_snapshot_before_claiming() {
    let Some(db) = database().await else { return };
    let mock = MockRest::start(
        vec![],
        ScriptedResponse::json(
            201,
            json!({"id":"700","guild_id":"100","parent_id":"200","type":2}),
        ),
    )
    .await;
    let rest = executor(&mock);
    let creator = event_voice_create_runtime::Creator {
        pool: db.pool(),
        rest: &rest,
        guild: "100",
        actor: 999,
        admission: two_bot_core::voice_create_admission::CreateAdmissionConfig::new(10, 10, 0)
            .unwrap(),
    };
    let starts = (OffsetDateTime::now_utc() + time::Duration::minutes(10)).unix_timestamp() * 1000;
    let event = json!({"event_id":"500","name":"Queued event","instance_starts_at_ms":starts,"binding_key_ms":starts,"voice_opt_out":false});
    let store = SettingsStore::new(db.pool());
    store
        .set(
            "100",
            EVENT_VOICE_OPT_OUT_KEY,
            Some(json!(["500"])),
            "moderator",
        )
        .await
        .unwrap();
    assert!(matches!(
        creator.create(&event, "200").await.unwrap(),
        event_voice_ticker::CreateResult::Ineligible
    ));
    store
        .set(
            "100",
            EVENT_VOICE_OPT_OUT_KEY,
            Some(json!(false)),
            "moderator",
        )
        .await
        .unwrap();
    assert!(matches!(
        creator.create(&event, "200").await,
        Err(ErrorClass::Configuration)
    ));
    assert!(mock.requests().is_empty());
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM event_voice_channels")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(count, 0);
    mock.shutdown().await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn opt_out_during_admission_releases_only_known_no_post_fences() {
    for setting in [json!(["500"]), json!(false)] {
        let Some(db) = database().await else { return };
        sqlx::raw_sql("CREATE FUNCTION notify_test_claim() RETURNS TRIGGER LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_notify('event_voice_test_claim','claimed'); RETURN NEW; END $$; CREATE TRIGGER notify_test_claim AFTER INSERT ON event_voice_channels FOR EACH ROW EXECUTE FUNCTION notify_test_claim();").execute(db.pool()).await.unwrap();
        let mut listener = sqlx::postgres::PgListener::connect_with(db.pool())
            .await
            .unwrap();
        listener.listen("event_voice_test_claim").await.unwrap();
        let mut capacity = db.pool().begin().await.unwrap();
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('voice_create:100',0))")
            .execute(&mut *capacity)
            .await
            .unwrap();
        let mock = MockRest::start(
            vec![],
            ScriptedResponse::json(
                201,
                json!({"id":"700","guild_id":"100","parent_id":"200","type":2}),
            ),
        )
        .await;
        let rest = executor(&mock);
        let creator = event_voice_create_runtime::Creator {
            pool: db.pool(),
            rest: &rest,
            guild: "100",
            actor: 999,
            admission: two_bot_core::voice_create_admission::CreateAdmissionConfig::new(10, 10, 0)
                .unwrap(),
        };
        let starts =
            (OffsetDateTime::now_utc() + time::Duration::minutes(10)).unix_timestamp() * 1000;
        let event = json!({"event_id":"500","name":"Queued event","instance_starts_at_ms":starts,"binding_key_ms":starts});
        let change = async {
            tokio::time::timeout(Duration::from_secs(5), listener.recv())
                .await
                .unwrap()
                .unwrap();
            SettingsStore::new(db.pool())
                .set(
                    "100",
                    EVENT_VOICE_OPT_OUT_KEY,
                    Some(setting.clone()),
                    "moderator",
                )
                .await
                .unwrap();
            capacity.commit().await.unwrap();
        };
        let (result, ()) = tokio::join!(creator.create(&event, "200"), change);
        if setting.is_array() {
            assert!(matches!(
                result,
                Ok(event_voice_ticker::CreateResult::Ineligible)
            ));
        } else {
            assert!(matches!(result, Err(ErrorClass::Configuration)));
        }
        assert!(mock.requests().is_empty());
        let state:(Option<String>,bool)=sqlx::query_as("SELECT e.owner_tick_lease,r.settled_at IS NOT NULL FROM event_voice_channels e CROSS JOIN voice_create_reservations r WHERE e.event_id='500'").fetch_one(db.pool()).await.unwrap();
        assert_eq!(state, (None, true));
        drop(listener);
        mock.shutdown().await;
        db.close().await.unwrap();
    }
}

#[tokio::test]
async fn source_does_not_guess_between_multiple_live_occurrences_or_clear_unknown_claims() {
    let Some(db) = database().await else { return };
    let original = OffsetDateTime::parse("2026-10-23T22:10:00.123Z", &Rfc3339).unwrap();
    claim_event_voice_channel(db.pool(), "500", original, original, None, "unknown-post")
        .await
        .unwrap();
    let rows = event_voice_source::load(db.pool(), "100", &json!([document("500", 2)]))
        .await
        .unwrap();
    assert!(rows[0]["voice_channel_id"].is_null());
    let lease: Option<String> = sqlx::query_scalar(
        "SELECT owner_tick_lease FROM event_voice_channels WHERE event_id='500'",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(lease.as_deref(), Some("unknown-post"));
    let second = original + time::Duration::days(7);
    claim_event_voice_channel(db.pool(), "500", second, second, None, "second-occurrence")
        .await
        .unwrap();
    assert_eq!(
        event_voice_source::load(db.pool(), "100", &json!([document("500", 2)]))
            .await
            .unwrap_err(),
        ErrorClass::Rest
    );
    db.close().await.unwrap();
}
