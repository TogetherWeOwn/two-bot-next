use super::*;
use serde_json::{from_slice, json, Value};

pub(super) fn id(value: u64) -> Id {
    Id(NonZeroU64::new(value).unwrap())
}

fn upsert<Row>(id: Id, value: Row) -> Change<Row> {
    Change::Upsert { id, value }
}

pub(super) fn now() -> WireTime {
    WireTime(OffsetDateTime::parse("2026-11-21T03:00:00+01:00", &Rfc3339).unwrap())
}

fn voice() -> Voice {
    Voice {
        user_id: id(300),
        channel_id: id(200),
    }
}

fn event() -> Event {
    Event {
        id: id(400),
        channel_id: Some(id(200)),
        status: EventStatus::Active,
        title: "Test event".into(),
        description: None,
        location: None,
        starts_at: now(),
        ends_at: None,
    }
}

#[test]
fn matches_the_site_voice_delta() {
    // The synthetic baseline in two-web-next's live-wire.test.ts (PR #2604).
    let update = Operation::Voice(upsert(id(300), voice()));
    let delta = encode_delta(id(100), 2, now(), &[update]).unwrap();
    assert_eq!(
        from_slice::<Value>(&delta).unwrap(),
        json!({
            "v":1, "guild_id":"100", "seq":2, "occurred_at":"2026-11-21T02:00:00Z",
            "ops":[{"topic":"voice", "op":"upsert", "id":"300", "value":{"userId":"300", "channelId":"200"}}]
        })
    );
}

#[test]
fn enforces_envelope_and_body_bounds() {
    let (guild_id, now) = (id(100), now());
    let remove = || Operation::Voice(Change::Remove { id: id(300) });
    let ops: Vec<_> = (0..129).map(|_| remove()).collect();
    for seq in [0, 9_007_199_254_740_992] {
        assert!(encode_delta(guild_id, seq, now, &ops[..1]).is_none());
    }
    let max = encode_delta(guild_id, 9_007_199_254_740_991, now, &ops[..128]).unwrap();
    let max = from_slice::<Value>(&max).unwrap();
    assert_eq!(max["seq"].as_u64(), Some(9_007_199_254_740_991));
    assert_eq!(
        max["ops"][0],
        json!({"topic":"voice", "op":"remove", "id":"300"})
    );
    assert!(encode_delta(guild_id, 1, now, &[]).is_none());
    assert!(encode_delta(guild_id, 1, now, &ops).is_none());

    let guild = Guild {
        owner_id: id(300),
        afk_channel_id: None,
    };
    for bad in [
        Operation::Voice(upsert(id(301), voice())),
        Operation::Guild(upsert(id(101), guild)),
        Operation::Guild(Change::Remove { id: guild_id }),
    ] {
        assert!(encode_delta(guild_id, 1, now, &[remove(), bad]).is_none());
    }

    let encode_events = |padding: usize| {
        let ops: Vec<_> = (0..64)
            .map(|i| {
                let mut row = event();
                row.description = Some("x".repeat(padding / 64 + usize::from(i < padding % 64)));
                Operation::Events(upsert(row.id, row))
            })
            .collect();
        encode_delta(guild_id, 1, now, &ops)
    };
    let padding = 65_536 - encode_events(0).unwrap().len();
    assert_eq!(encode_events(padding).unwrap().len(), 65_536);
    assert!(encode_events(padding + 1).is_none());
}

#[test]
fn rejects_invalid_rows_in_delta_batches() {
    let (guild_id, now) = (id(100), now());
    let mut channel = Channel {
        id: id(200),
        name: "😀".repeat(50),
        kind: ChannelKind::Voice,
        parent_id: None,
        position: Some(1),
        overwrites: vec![],
    };
    let change = |row: &Channel| Operation::Channels(upsert(row.id, row.clone()));
    assert!(encode_delta(guild_id, 1, now, &[change(&channel)]).is_some());
    channel.name.push('a');
    assert!(encode_delta(guild_id, 1, now, &[change(&channel)]).is_none());
    channel.name = "Test room".into();
    let overwrite = Overwrite {
        kind: OverwriteKind::Role,
        id: guild_id,
        allow: Permissions(0),
        deny: Permissions(0),
    };
    channel.overwrites = vec![overwrite.clone(), overwrite];
    assert!(encode_delta(guild_id, 1, now, &[change(&channel)]).is_none());

    let member = Member {
        user_id: id(300),
        role_ids: vec![guild_id, guild_id],
        nick: None,
        bot: false,
        pending: false,
    };
    let upsert_member = Operation::Members(upsert(member.user_id, member));
    assert!(encode_delta(guild_id, 1, now, &[upsert_member]).is_none());

    let mut event = event();
    event.ends_at = Some(WireTime(now.0 + time::Duration::microseconds(999)));
    let upsert_event = Operation::Events(upsert(event.id, event));
    assert!(encode_delta(guild_id, 1, now, &[upsert_event]).is_none());
}
