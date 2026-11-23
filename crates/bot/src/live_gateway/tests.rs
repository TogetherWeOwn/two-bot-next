use super::*;
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};
use twilight_model::id::marker::ScheduledEventMarker;

const GUILD: GuildId = Id::new(10);
const AT: WireTime = WireTime(time::OffsetDateTime::UNIX_EPOCH);
const READY: &str = r#"{"op":0,"t":"READY","d":{}}"#;
const CREATE: &str = include_str!("../../tests/fixtures/live_gateway_guild_create.json");
const VOICE: &str = include_str!("../../tests/fixtures/live_gateway_voice_state_update.json");

fn event_row(id: u64) -> Value {
    let mut row: Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/live_gateway_scheduled_event.json"
    ))
    .expect("scheduled event fixture");
    row["id"] = json!(id.to_string());
    row
}

fn event(live: &mut LiveGateway, kind: &str, row: Value) {
    let frame = json!({"op": 0, "s": 1, "t": kind, "d": row}).to_string();
    assert!(
        twilight_gateway::parse(frame.clone(), twilight_gateway::EventTypeFlags::all())
            .is_ok_and(|parsed| parsed.is_some()),
        "{kind} fixture must pass the shard parser"
    );
    receive(live, &frame);
}

fn receive(live: &mut LiveGateway, raw: &str) {
    live.receive(raw, AT, |_| panic!("unexpected member request"));
}

fn begin(live: &mut LiveGateway) -> String {
    begin_with(live, CREATE)
}

fn begin_with(live: &mut LiveGateway, create: &str) -> String {
    let mut nonce = None;
    live.receive(create, AT, |request| {
        assert_eq!(request.d.guild_id, GUILD);
        nonce.clone_from(&request.d.nonce);
        true
    });
    nonce.expect("fresh member request")
}

fn complete(live: &mut LiveGateway, nonce: &str) {
    receive(live, &json!({"op":0,"t":"GUILD_MEMBERS_CHUNK","d":{"guild_id":"10","nonce":nonce,
        "chunk_index":0,"chunk_count":1,"members":[{"user":{"id":"31"},"roles":[],"pending":false}]}}).to_string());
}

fn decoded(body: &[u8]) -> Value {
    serde_json::from_slice(body).expect("wire delta")
}

#[test]
fn default_dispatches_and_cold_resume_make_no_requests() {
    let mut disabled = LiveGateway::default();
    assert!(!disabled.enabled());
    for frame in [READY, CREATE, VOICE, "invalid"] {
        receive(&mut disabled, frame);
    }
    event(&mut disabled, "GUILD_SCHEDULED_EVENT_CREATE", event_row(90));
    assert!(!disabled.enabled());
    let (mut live, _) = LiveGateway::for_guild(GUILD, 0);
    receive(&mut live, r#"{"op":0,"t":"RESUMED","d":{}}"#);
    receive(&mut live, CREATE);
}

#[tokio::test(start_paused = true)]
async fn scheduled_events_share_voice_order_and_emit_only_allowlisted_fields() {
    let (mut live, receiver) = LiveGateway::for_guild(GUILD, 40);
    receive(&mut live, READY);
    event(&mut live, "GUILD_SCHEDULED_EVENT_CREATE", event_row(90));
    let nonce = begin(&mut live);
    event(&mut live, "GUILD_SCHEDULED_EVENT_CREATE", event_row(90));
    complete(&mut live, &nonce);
    let baseline = live
        .0
        .as_ref()
        .and_then(|intake| intake.active.as_ref())
        .expect("completed baseline");
    assert_eq!(baseline.events.len(), 1);
    let joined_event = wire::Id(Id::<ScheduledEventMarker>::new(90).into_nonzero());
    assert!(baseline.events.contains(&joined_event));

    let mut foreign = event_row(92);
    foreign["guild_id"] = json!("11");
    event(&mut live, "GUILD_SCHEDULED_EVENT_CREATE", foreign);
    event(
        &mut live,
        "GUILD_SCHEDULED_EVENT_USER_ADD",
        json!({"guild_id": "10", "user_id": "31", "guild_scheduled_event_id": "91"}),
    );
    event(&mut live, "GUILD_SCHEDULED_EVENT_CREATE", event_row(91));
    receive(&mut live, VOICE);
    let mut active = event_row(91);
    active["status"] = json!(2);
    active["channel_id"] = json!("21");
    active["entity_type"] = json!(1);
    event(&mut live, "GUILD_SCHEDULED_EVENT_UPDATE", active.clone());
    active["status"] = json!(3);
    event(&mut live, "GUILD_SCHEDULED_EVENT_UPDATE", active);
    event(&mut live, "GUILD_SCHEDULED_EVENT_DELETE", event_row(90));

    let (tx, mut rx) = mpsc::unbounded_channel();
    let worker = receiver.run(async move |body| {
        tx.send(decoded(body)).expect("fixture receiver");
        Delivery::Accepted
    });
    tokio::join!(worker, async {
        let mut value = json!({
            "id": "91", "channelId": "20", "status": "scheduled", "title": "Fixture event",
            "description": null, "location": null, "startsAt": "2026-11-21T13:00:00Z", "endsAt": null
        });
        let created = json!({"topic": "events", "op": "upsert", "id": "91", "value": value});
        value["channelId"] = json!("21");
        value["status"] = json!("active");
        let updated = json!({"topic": "events", "op": "upsert", "id": "91", "value": value});
        let expected = [
            created,
            json!({"topic": "voice", "op": "upsert", "id": "31",
                   "value": {"userId": "31", "channelId": "20"}}),
            updated,
            json!({"topic": "events", "op": "remove", "id": "91"}),
            json!({"topic": "events", "op": "remove", "id": "90"}),
        ];
        for (seq, op) in (41..).zip(expected) {
            let delta = tokio::time::timeout(Duration::from_secs(1), rx.recv())
                .await
                .expect("timely delta")
                .expect("queued delta");
            assert_eq!(
                delta,
                json!({"v": 1, "guild_id": "10", "seq": seq,
                       "occurred_at": "1970-01-01T00:00:00Z", "ops": [op]})
            );
        }
        assert!(rx.try_recv().is_err());
        drop(live);
    });
}

#[test]
fn scheduled_event_limit_counts_baseline_rows_and_releases_removed_rows() {
    let (mut live, _receiver) = LiveGateway::for_guild(GUILD, 0);
    receive(&mut live, READY);
    let mut create = decoded(CREATE.as_bytes());
    create["d"]["guild_scheduled_events"] = json!((1000..1100).map(event_row).collect::<Vec<_>>());
    let nonce = begin_with(&mut live, &create.to_string());
    complete(&mut live, &nonce);

    event(&mut live, "GUILD_SCHEDULED_EVENT_UPDATE", event_row(1000));
    event(&mut live, "GUILD_SCHEDULED_EVENT_DELETE", event_row(1000));
    event(&mut live, "GUILD_SCHEDULED_EVENT_CREATE", event_row(1100));
    assert!(live.0.as_ref().expect("intake").active.is_some());
    event(&mut live, "GUILD_SCHEDULED_EVENT_CREATE", event_row(1101));
    receive(&mut live, VOICE);
    let intake = live.0.as_ref().expect("intake");
    assert!(intake.active.is_none());
    assert_eq!(intake.seq, 3);
}

#[tokio::test(start_paused = true)]
async fn voice_only_is_encoded_once_in_receive_order_and_retried_unchanged() {
    let (mut live, receiver) = LiveGateway::for_guild(GUILD, 40);
    receive(&mut live, VOICE);
    receive(&mut live, READY);
    let nonce = begin(&mut live);
    receive(&mut live, VOICE);
    complete(&mut live, &nonce);
    receive(
        &mut live,
        r#"{"op":0,"t":"MESSAGE_CREATE","d":{"content":"private message"}}"#,
    );
    receive(&mut live, &VOICE.replace(r#""10""#, r#""11""#));
    receive(&mut live, VOICE);
    receive(&mut live, &VOICE.replace(r#""20""#, r#""21""#));
    receive(&mut live, &VOICE.replace(r#""20""#, "null"));

    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut retry = true;
    let worker = receiver.run(async move |body| {
        tx.send(body.to_vec()).expect("fixture receiver");
        if std::mem::take(&mut retry) {
            Delivery::Retry
        } else {
            Delivery::Accepted
        }
    });
    tokio::join!(worker, async {
        let first = rx.recv().await.expect("join");
        assert_eq!(rx.recv().await.expect("retry"), first);
        let expected = include_str!("../../tests/fixtures/live_gateway_deltas.json");
        let moved = rx.recv().await.expect("move");
        let left = rx.recv().await.expect("leave");
        assert_eq!(
            json!([decoded(&first), decoded(&moved), decoded(&left)]),
            decoded(expected.as_bytes())
        );
        assert!(rx.try_recv().is_err());
        drop(live);
    });
}

#[tokio::test(start_paused = true)]
async fn rejected_or_superseded_baselines_cannot_send() {
    let (mut live, receiver) = LiveGateway::for_guild(GUILD, 0);
    let scheduled_event = |field: &str, value: Value| {
        let mut row = event_row(91);
        row[field] = value;
        json!({"op": 0, "t": "GUILD_SCHEDULED_EVENT_UPDATE", "d": row}).to_string()
    };
    let invalidations = [
        VOICE.replace(r#""20""#, r#""22""#),
        VOICE.replace(r#""31""#, r#""99""#),
        VOICE.replace(r#""guild_id":"10""#, r#""guild_id":"10","guild_id":"11""#),
        r#"{"op":0,"t":"GUILD_MEMBER_REMOVE","d":{"guild_id":"10","user":{"id":"31"}}}"#.into(),
        scheduled_event("channel_id", json!("22")),
        scheduled_event("channel_id", json!("99")),
        scheduled_event("privacy_level", json!(99)),
        scheduled_event("entity_metadata", json!(["forbidden shape"])),
        r#"{"op":0,"t":"GUILD_SCHEDULED_EVENT_DELETE","d":{"guild_id":"10","id":"91","id":"90"}}"#
            .into(),
    ];
    let current_seq = invalidations.len() + 1;
    for invalid in invalidations {
        receive(&mut live, READY);
        let nonce = begin(&mut live);
        complete(&mut live, &nonce);
        receive(&mut live, VOICE);
        receive(&mut live, &invalid);
        receive(&mut live, VOICE);
    }
    let stale = begin(&mut live);
    let current = begin(&mut live);
    complete(&mut live, &stale);
    receive(&mut live, VOICE);
    complete(&mut live, &current);
    receive(&mut live, VOICE);

    let (tx, mut rx) = mpsc::unbounded_channel();
    let (attempt, cancelled) = oneshot::channel::<()>();
    let mut attempt = Some(attempt);
    let worker = receiver.run(async move |body| {
        let _in_flight = attempt.take();
        tx.send(decoded(body)).expect("fixture receiver");
        std::future::pending().await
    });
    let (_, retired_at) = tokio::join!(worker, async {
        assert_eq!(rx.recv().await.expect("current delta")["seq"], current_seq);
        let retired_at = Instant::now();
        live.disconnect();
        assert!(cancelled.await.is_err());
        drop(live);
        retired_at
    });
    assert_eq!(retired_at.elapsed(), Duration::ZERO);
    assert!(rx.try_recv().is_err());
}
