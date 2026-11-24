use super::*;
use crate::tuning_test_support::Capture;
use futures_util::FutureExt;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};
use tracing::instrument::WithSubscriber;
use twilight_model::id::marker::ScheduledEventMarker;

const GUILD: GuildId = Id::new(10);
const AT: WireTime = WireTime(time::OffsetDateTime::UNIX_EPOCH);
const READY: &str = r#"{"op":0,"t":"READY","d":{}}"#;
const CREATE: &str = include_str!("../../tests/fixtures/live_gateway_guild_create.json");
const VOICE: &str = include_str!("../../tests/fixtures/live_gateway_voice_state_update.json");
const TOPOLOGY: &[u8] = include_bytes!("../../tests/fixtures/live_gateway_topology.json");

#[derive(Clone)]
struct FakeWatermark(Arc<Mutex<WatermarkState>>);

struct WatermarkState {
    seq: u64,
    ack: Option<Ack>,
}

enum Ack {
    Lost,
    Pending,
}

impl FakeWatermark {
    fn new(seq: u64) -> Self {
        Self(Arc::new(Mutex::new(WatermarkState { seq, ack: None })))
    }
}

impl SequenceWatermark for FakeWatermark {
    async fn read(&self, guild: wire::Id) -> Result<u64, ReservationError> {
        assert_eq!(guild.0, GUILD.into_nonzero());
        Ok(self.0.lock().expect("fake watermark").seq)
    }

    async fn reserve(
        &self,
        guild: wire::Id,
        expected: u64,
        next: u64,
    ) -> Result<(), ReservationError> {
        assert_eq!(guild.0, GUILD.into_nonzero());
        assert!(next <= wire::MAX_SEQ && matches!(next.checked_sub(expected), Some(1 | 2)));
        let ack = {
            let mut state = self.0.lock().expect("fake watermark");
            if state.seq != expected {
                return Err(ReservationError::Conflict);
            }
            state.seq = next;
            state.ack.take()
        };
        match ack {
            Some(Ack::Pending) => std::future::pending().await,
            Some(Ack::Lost) => Err(ReservationError::Storage),
            None => Ok(()),
        }
    }
}

fn event_row(id: u64) -> Value {
    let mut row: Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/live_gateway_scheduled_event.json"
    ))
    .expect("scheduled event fixture");
    row["id"] = json!(id.to_string());
    row
}

fn event(live: &mut LiveGateway<impl SequenceWatermark>, kind: &str, row: Value) {
    let frame = json!({"op": 0, "s": 1, "t": kind, "d": row}).to_string();
    assert!(
        twilight_gateway::parse(frame.clone(), twilight_gateway::EventTypeFlags::all())
            .is_ok_and(|parsed| parsed.is_some()),
        "{kind} fixture must pass the shard parser"
    );
    receive(live, &frame);
}

fn receive(live: &mut LiveGateway<impl SequenceWatermark>, raw: &str) {
    live.receive(raw, AT, |_| panic!("unexpected member request"))
        .now_or_never()
        .expect("immediate fixture storage");
}

fn begin(live: &mut LiveGateway<FakeWatermark>) -> String {
    begin_with(live, CREATE)
}

fn begin_with(live: &mut LiveGateway<FakeWatermark>, create: &str) -> String {
    let mut nonce = None;
    live.receive(create, AT, |request| {
        assert_eq!(request.d.guild_id, GUILD);
        nonce.clone_from(&request.d.nonce);
        true
    })
    .now_or_never()
    .expect("baseline collection starts immediately");
    nonce.expect("fresh member request")
}

fn complete(live: &mut LiveGateway<FakeWatermark>, nonce: &str) {
    receive(live, &chunk(nonce));
}

fn chunk(nonce: &str) -> String {
    json!({"op":0,"t":"GUILD_MEMBERS_CHUNK","d":{"guild_id":"10","nonce":nonce,
        "chunk_index":0,"chunk_count":1,"members":[{"user":{"id":"31"},"roles":[],"pending":false}]}}).to_string()
}

fn decoded(body: &[u8]) -> Value {
    serde_json::from_slice(body).expect("wire delta")
}

#[tokio::test(start_paused = true)]
async fn conflict_or_lost_ack_requires_a_new_covering_baseline() {
    for lose_ack in [false, true] {
        let watermark = FakeWatermark::new(0);
        let (mut live, receiver) = LiveGateway::for_guild(GUILD, watermark.clone());
        receive(&mut live, READY);
        let old = begin(&mut live);
        complete(&mut live, &old);
        assert!(live.recovery().is_some());
        receive(&mut live, VOICE); // Queue a confirmed delta at 3.
        {
            let mut store = watermark.0.lock().expect("fake watermark");
            assert_eq!(store.seq, 3);
            if lose_ack {
                store.ack = Some(Ack::Lost);
            } else {
                store.seq = 4; // A competing reservation won the same target.
            }
        }
        receive(&mut live, VOICE);
        receive(&mut live, VOICE);
        complete(&mut live, &old);
        assert!(live.0.as_ref().expect("intake").active.is_none());
        assert_eq!(watermark.0.lock().expect("fake watermark").seq, 4);

        let fresh = begin(&mut live);
        complete(&mut live, &old);
        assert!(live.recovery().is_none());
        complete(&mut live, &fresh);
        let (recovery, generation) = live.recovery().expect("replacement published baseline");
        assert!(recovery.is_current(generation));
        let baseline = recovery.baseline().expect("replacement snapshot");
        let snapshot = decoded(&baseline.snapshot);
        assert_eq!(snapshot["seq"], 6);
        assert_eq!(snapshot["topics"]["members"][0]["userId"], "31");
        receive(&mut live, VOICE);
        assert!(live.recovery().is_none());
        assert_eq!(watermark.0.lock().expect("fake watermark").seq, 7);

        let mut received = Vec::new();
        let worker = receiver.run(async |body| {
            received.push(decoded(body));
            Delivery::Accepted
        });
        assert!(timeout(Duration::from_secs(1), worker).await.is_err());
        let [marker, delta] = received.as_slice() else {
            panic!("only the rebuilt generation should reach the sink: {received:?}");
        };
        assert_eq!(marker["seq"], snapshot["seq"]);
        assert_eq!(marker["ops"][0]["topic"], "guild");
        assert_eq!(delta["seq"], 7);
        assert_eq!(delta["ops"][0]["topic"], "voice");
    }
}

#[tokio::test(start_paused = true)]
async fn cancelling_or_timing_out_a_committed_reservation_withdraws_delivery() {
    let watermark = FakeWatermark::new(0);
    let (mut live, receiver) = LiveGateway::for_guild(GUILD, watermark.clone());
    receive(&mut live, READY);
    let first = begin(&mut live);
    watermark.0.lock().expect("fake watermark").ack = Some(Ack::Pending);
    live.receive(&chunk(&first), AT, |_| false).await;
    assert!(live.recovery().is_none());
    assert_eq!(watermark.0.lock().expect("fake watermark").seq, 2);

    let second = begin(&mut live);
    complete(&mut live, &second);
    assert!(live.recovery().is_some());
    watermark.0.lock().expect("fake watermark").ack = Some(Ack::Pending);
    assert!(live.receive(VOICE, AT, |_| false).now_or_never().is_none());
    assert!(live.0.as_ref().expect("intake").active.is_none());
    receive(&mut live, VOICE);
    assert!(live.recovery().is_none());
    assert_eq!(watermark.0.lock().expect("fake watermark").seq, 5);

    let fresh = begin(&mut live);
    complete(&mut live, &fresh);
    let mut received = Vec::new();
    let worker = receiver.run(async |body| {
        received.push(decoded(body)["seq"].as_u64().expect("wire sequence"));
        Delivery::Accepted
    });
    assert!(timeout(Duration::from_secs(1), worker).await.is_err());
    assert_eq!(received, [7]);
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
    let (mut live, _) = LiveGateway::for_guild(GUILD, FakeWatermark::new(0));
    receive(&mut live, r#"{"op":0,"t":"RESUMED","d":{}}"#);
    receive(&mut live, CREATE);
}

#[tokio::test(start_paused = true)]
async fn scheduled_events_share_voice_order_and_emit_only_allowlisted_fields() {
    let (mut live, receiver) = LiveGateway::for_guild(GUILD, FakeWatermark::new(38));
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
    assert!(baseline.events.contains_key(&joined_event));

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
        assert_eq!(rx.recv().await.expect("recovery marker")["seq"], 40);
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

#[tokio::test]
async fn scheduled_event_limit_counts_baseline_rows_and_releases_removed_rows() {
    let watermark = FakeWatermark::new(0);
    let (mut live, _receiver) = LiveGateway::for_guild(GUILD, watermark.clone());
    receive(&mut live, READY);
    let mut create = decoded(CREATE.as_bytes());
    let mut events = (1000..1100).map(event_row).collect::<Vec<_>>();
    events[0]["channel_id"] = json!("21");
    events[0]["entity_type"] = json!(1);
    create["d"]["guild_scheduled_events"] = json!(events);
    let nonce = begin_with(&mut live, &create.to_string());
    complete(&mut live, &nonce);

    event(&mut live, "GUILD_SCHEDULED_EVENT_UPDATE", event_row(1001));
    event(
        &mut live,
        "CHANNEL_DELETE",
        json!({"guild_id":"10","id":"21","type":13}),
    );
    event(&mut live, "GUILD_SCHEDULED_EVENT_DELETE", event_row(1001));
    event(&mut live, "GUILD_SCHEDULED_EVENT_CREATE", event_row(1100));
    event(&mut live, "GUILD_SCHEDULED_EVENT_CREATE", event_row(1101));
    assert!(live.0.as_ref().expect("intake").active.is_some());
    event(&mut live, "GUILD_SCHEDULED_EVENT_CREATE", event_row(1102));
    receive(&mut live, VOICE);
    let intake = live.0.as_ref().expect("intake");
    assert!(intake.active.is_none());
    assert_eq!(watermark.0.lock().expect("fake watermark").seq, 7);
}

#[tokio::test(start_paused = true)]
async fn topology_and_roster_share_receive_order_with_voice_and_events() {
    let fixture = decoded(TOPOLOGY);
    let (mut live, receiver) = LiveGateway::for_guild(GUILD, FakeWatermark::new(38));
    receive(&mut live, READY);
    let nonce = begin(&mut live);
    complete(&mut live, &nonce);
    let mut disabled = LiveGateway::default();
    for row in fixture["dispatches"].as_array().expect("dispatches") {
        let kind = row[0].as_str().expect("dispatch type");
        event(&mut disabled, kind, row[1].clone());
        event(&mut live, kind, row[1].clone());
    }
    assert!(!disabled.enabled());

    let mut deltas = Vec::new();
    let worker = receiver.run(async |body| {
        deltas.push(decoded(body));
        Delivery::Accepted
    });
    assert!(tokio::time::timeout(Duration::from_secs(1), worker)
        .await
        .is_err());
    assert_eq!(deltas[0]["seq"], 40);
    assert_eq!(&deltas[1..], fixture["deltas"].as_array().expect("deltas"));
}

#[tokio::test(start_paused = true)]
async fn voice_only_is_encoded_once_in_receive_order_and_retried_unchanged() {
    let (mut live, receiver) = LiveGateway::for_guild(GUILD, FakeWatermark::new(38));
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
        if decoded(body)["seq"] == 41 && std::mem::take(&mut retry) {
            Delivery::Retry
        } else {
            Delivery::Accepted
        }
    });
    tokio::join!(worker, async {
        assert_eq!(
            decoded(&rx.recv().await.expect("recovery marker"))["seq"],
            40
        );
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
    let (mut live, receiver) = LiveGateway::for_guild(GUILD, FakeWatermark::new(0));
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
    let fixture = decoded(TOPOLOGY);
    let extra = fixture["invalid"].as_array().expect("invalid dispatches");
    // Each completed old generation reserves a marker (+2) and a voice delta (+1).
    let current_seq = (invalidations.len() + extra.len() + 1) * 3 + 2;
    let invalidations = invalidations
        .into_iter()
        .chain(extra.iter().map(Value::to_string));
    for invalid in invalidations {
        receive(&mut live, READY);
        let nonce = begin_with(&mut live, &fixture["rejection_baseline"].to_string());
        complete(&mut live, &nonce);
        receive(&mut live, VOICE);
        receive(&mut live, &invalid);
        assert!(live.0.as_ref().expect("intake").active.is_none());
        receive(&mut live, VOICE);
    }
    let stale = begin(&mut live);
    complete(&mut live, &stale);
    receive(&mut live, VOICE);
    let member = fixture["missing_screening"].clone();
    event(&mut live, "GUILD_MEMBER_ADD", member);
    assert!(live.0.as_ref().expect("intake").active.is_none());
    receive(&mut live, VOICE);
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

#[tokio::test(start_paused = true)]
async fn disconnect_discards_retry_and_backlog_before_a_fresh_baseline_delivers() {
    let (mut live, receiver) = LiveGateway::for_guild(GUILD, FakeWatermark::new(0));
    receive(&mut live, READY);
    let nonce = begin(&mut live);
    complete(&mut live, &nonce);
    receive(&mut live, VOICE);
    receive(&mut live, VOICE);

    let capture = Capture::default();
    let (tx, mut rx) = mpsc::channel(4);
    let worker = receiver.run(async move |body| {
        let seq = decoded(body)["seq"].as_u64().expect("wire sequence");
        tx.try_send(seq).expect("bounded fixture attempts");
        if seq == 3 {
            Delivery::Retry
        } else {
            Delivery::Accepted
        }
    });
    let rebuild = async {
        assert_eq!(rx.recv().await, Some(2));
        assert_eq!(rx.recv().await, Some(3));
        let retired_at = Instant::now();
        live.disconnect();
        live.disconnect();
        receive(&mut live, r#"{"op":0,"t":"RESUMED","d":{}}"#);
        receive(&mut live, CREATE);
        receive(&mut live, VOICE);
        receive(&mut live, READY);
        let nonce = begin(&mut live);
        complete(&mut live, &nonce);
        receive(&mut live, VOICE);
        assert_eq!(rx.recv().await, Some(6));
        assert_eq!(rx.recv().await, Some(7));
        assert_eq!(retired_at.elapsed(), Duration::ZERO);
        drop(live);
        assert_eq!(rx.recv().await, None);
    };
    tokio::time::timeout(Duration::from_secs(1), async {
        tokio::join!(worker, rebuild);
    })
    .with_subscriber(capture.subscriber())
    .await
    .expect("retirement and rebuild complete");
    let records = capture.records();
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0].fields,
        [
            ("msg".into(), "live_generation_retired".into()),
            ("reason".into(), "disconnect".into()),
        ]
        .into()
    );
}

#[tokio::test(start_paused = true)]
async fn shutdown_drops_in_flight_and_buffered_work_with_intake_still_alive() {
    let (mut live, receiver) = LiveGateway::for_guild(GUILD, FakeWatermark::new(0));
    receive(&mut live, READY);
    let nonce = begin(&mut live);
    complete(&mut live, &nonce);
    receive(&mut live, VOICE);
    receive(&mut live, VOICE);

    let (shutdown, stop) = oneshot::channel();
    let (tx, mut rx) = mpsc::channel(2);
    let (in_flight, cancelled) = oneshot::channel::<()>();
    let mut in_flight = Some(in_flight);
    let worker = receiver.run_until(
        async { stop.await.expect("shutdown signal") },
        async move |body| {
            let _drop_signal = in_flight.take();
            tx.try_send(body.to_vec())
                .expect("bounded fixture attempts");
            std::future::pending().await
        },
    );
    let stop_worker = async {
        let body = rx.recv().await.expect("first attempt");
        assert_eq!(decoded(&body)["seq"], 2);
        let stopped_at = Instant::now();
        shutdown.send(()).expect("worker waiting for shutdown");
        assert!(cancelled.await.is_err());
        assert_eq!(rx.recv().await, None);
        assert_eq!(stopped_at.elapsed(), Duration::ZERO);
        body
    };
    let (_, body) = tokio::time::timeout(Duration::from_secs(1), async {
        tokio::join!(worker, stop_worker)
    })
    .await
    .expect("worker shuts down while the intake still owns the sender");

    let intake = live.0.as_ref().expect("intake");
    let generation = intake.active.as_ref().expect("active baseline");
    assert!(!intake.sender.offer(QueuedDelta {
        body,
        generation: generation.lifetime.subscribe(),
    }));
}
