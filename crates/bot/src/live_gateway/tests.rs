use super::*;
use serde_json::{json, Value};
use tokio::sync::mpsc;

const GUILD: GuildId = Id::new(10);
const AT: WireTime = WireTime(time::OffsetDateTime::UNIX_EPOCH);
const READY: &str = r#"{"op":0,"t":"READY","d":{}}"#;
const CREATE: &str = include_str!("../../tests/fixtures/live_gateway_guild_create.json");
const VOICE: &str = include_str!("../../tests/fixtures/live_gateway_voice_state_update.json");

fn receive(live: &mut LiveGateway, raw: &str) {
    live.receive(raw, AT, |_| panic!("unexpected member request"));
}

fn begin(live: &mut LiveGateway) -> String {
    let mut nonce = None;
    live.receive(CREATE, AT, |request| {
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

fn arm(live: &mut LiveGateway) {
    receive(live, READY);
    let nonce = begin(live);
    complete(live, &nonce);
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
    let (mut live, _) = LiveGateway::for_guild(GUILD, 0);
    receive(&mut live, r#"{"op":0,"t":"RESUMED","d":{}}"#);
    receive(&mut live, CREATE);
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
    for invalid in [
        VOICE.replace(r#""20""#, r#""22""#),
        VOICE.replace(r#""31""#, r#""99""#),
        VOICE.replace(r#""guild_id":"10""#, r#""guild_id":"10","guild_id":"11""#),
        r#"{"op":0,"t":"GUILD_MEMBER_REMOVE","d":{"guild_id":"10","user":{"id":"31"}}}"#.into(),
    ] {
        arm(&mut live);
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
    let worker = receiver.run(async move |body| {
        tx.send(decoded(body)).expect("fixture receiver");
        std::future::pending().await
    });
    let (_, retired_at) = tokio::join!(worker, async {
        assert_eq!(rx.recv().await.expect("current delta")["seq"], 5);
        let retired_at = Instant::now();
        live.disconnect();
        drop(live);
        retired_at
    });
    assert_eq!(retired_at.elapsed(), Duration::ZERO);
    assert!(rx.try_recv().is_err());
}
