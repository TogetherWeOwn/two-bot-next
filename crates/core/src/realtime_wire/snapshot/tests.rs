use super::*;
use crate::realtime_wire::{
    tests::{id, now},
    EventStatus, Overwrite, Permissions,
};
use serde_json::{from_slice, json, Value};

fn baseline() -> (Id, WireTime, Topics) {
    let (guild_id, room_id, user_id, event_id) = (id(100), id(200), id(300), id(400));
    let now = now();
    let topics = Topics {
        guild: Guild {
            owner_id: user_id,
            afk_channel_id: None,
        },
        channels: vec![Channel {
            id: room_id,
            name: "Test room".into(),
            kind: ChannelKind::Voice,
            parent_id: None,
            position: Some(1),
            overwrites: vec![],
        }],
        roles: vec![Role {
            id: guild_id,
            permissions: Permissions(1024),
        }],
        members: vec![Member {
            user_id,
            role_ids: vec![],
            nick: None,
            bot: false,
            pending: false,
        }],
        voice: vec![Voice {
            user_id,
            channel_id: room_id,
        }],
        events: vec![Event {
            id: event_id,
            channel_id: Some(room_id),
            status: EventStatus::Active,
            title: "Test event".into(),
            description: None,
            location: None,
            starts_at: now,
            ends_at: None,
        }],
    };
    (guild_id, now, topics)
}

fn example_topics_json() -> Value {
    let utc = "2026-11-21T02:00:00Z";
    json!({
        "guild":{"ownerId":"300", "afkChannelId":null},
        "channels":[{"id":"200", "name":"Test room", "kind":"voice", "parentId":null, "position":1, "overwrites":[]}],
        "roles":[{"id":"100", "permissions":"1024"}],
        "members":[{"userId":"300", "roleIds":[], "nick":null, "bot":false, "pending":false}],
        "voice":[{"userId":"300", "channelId":"200"}],
        "events":[{"id":"400", "channelId":"200", "status":"active", "title":"Test event", "description":null, "location":null, "startsAt":utc, "endsAt":null}]
    })
}

#[test]
fn matches_the_direct_recovery_reply() {
    // Match the synthetic signed recovery reply in the site's live-wire.test.ts.
    let (guild_id, now, topics) = baseline();
    let body = encode_snapshot(guild_id, 8, now, &topics).unwrap();
    assert_eq!(
        from_slice::<Value>(&body).unwrap(),
        json!({
            "v": 1, "guild_id": "100", "seq": 8, "observed_at": "2026-11-21T02:00:00Z",
            "topics": example_topics_json(),
        })
    );
    assert!(encode_snapshot(guild_id, 0, now, &topics).is_some());
    assert!(encode_snapshot(guild_id, 9_007_199_254_740_991, now, &topics).is_some());
    assert!(encode_snapshot(guild_id, 9_007_199_254_740_992, now, &topics).is_none());
}

#[test]
fn enforces_the_exact_snapshot_body_limit() {
    let (guild_id, now, mut topics) = baseline();
    topics.roles.extend((600..700).map(|n| Role {
        id: id(n),
        permissions: Permissions(0),
    }));
    let role_ids: Vec<_> = (600..700).map(id).collect();
    topics.members.extend((1000..2400).map(|n| Member {
        user_id: id(n),
        role_ids: role_ids.clone(),
        nick: Some("x".into()),
        bot: false,
        pending: false,
    }));
    let initial = encode_snapshot(guild_id, 1, now, &topics).unwrap();
    let cap = 1_048_576;
    let padding = cap - initial.len();
    let members = &mut topics.members[1..];
    let count = members.len();
    for (index, row) in members.iter_mut().enumerate() {
        row.nick = Some("x".repeat(1 + padding / count + usize::from(index < padding % count)));
    }
    assert_eq!(
        encode_snapshot(guild_id, 1, now, &topics).unwrap().len(),
        cap
    );
    topics.members[1].nick.as_mut().unwrap().push('x');
    assert!(valid_topics(guild_id, &topics));
    assert!(encode_snapshot(guild_id, 1, now, &topics).is_none());
}

#[test]
fn rejects_inconsistent_or_over_limit_snapshot_state() {
    let (guild_id, now, mut topics) = baseline();
    let room = topics.channels[0].clone();
    topics.channels.push(room.clone());
    assert!(encode_snapshot(guild_id, 2, now, &topics).is_none());
    topics.channels.pop();

    topics.channels[0].parent_id = Some(room.id);
    assert!(encode_snapshot(guild_id, 2, now, &topics).is_none());
    topics.channels[0].parent_id = None;
    topics.guild.afk_channel_id = Some(id(999));
    assert!(encode_snapshot(guild_id, 2, now, &topics).is_none());
    topics.guild.afk_channel_id = Some(room.id);
    assert!(encode_snapshot(guild_id, 2, now, &topics).is_some());

    topics.roles.clear();
    assert!(encode_snapshot(guild_id, 2, now, &topics).is_none());
    topics.roles.extend((1..=251).map(|n| Role {
        id: id(n),
        permissions: Permissions(0),
    }));
    assert!(encode_snapshot(guild_id, 2, now, &topics).is_none());
    topics.roles.pop();
    assert!(encode_snapshot(guild_id, 2, now, &topics).is_some());
    topics.members[0].role_ids.push(id(999));
    assert!(encode_snapshot(guild_id, 2, now, &topics).is_none());
}

#[test]
fn enforces_channel_member_and_event_counts_independently() {
    let (guild_id, now, mut topics) = baseline();
    let room = topics.channels[0].clone();
    topics.channels.extend((1000..1500).map(|n| Channel {
        id: id(n),
        ..room.clone()
    }));
    assert!(encode_snapshot(guild_id, 2, now, &topics).is_none());
    topics.channels.pop();
    assert!(encode_snapshot(guild_id, 2, now, &topics).is_some());
    topics.channels.truncate(1);

    let member = topics.members[0].clone();
    topics.members.extend((1000..11_000).map(|n| Member {
        user_id: id(n),
        ..member.clone()
    }));
    assert!(encode_snapshot(guild_id, 2, now, &topics).is_none());
    topics.members.pop();
    assert!(encode_snapshot(guild_id, 2, now, &topics).is_some());
    topics.members.truncate(1);

    let event = topics.events[0].clone();
    topics.events.extend((1000..1100).map(|n| Event {
        id: id(n),
        ..event.clone()
    }));
    assert!(encode_snapshot(guild_id, 2, now, &topics).is_none());
    topics.events.pop();
    assert!(encode_snapshot(guild_id, 2, now, &topics).is_some());
}

#[test]
fn rejects_duplicate_keys_in_each_non_channel_collection() {
    let (guild_id, now, mut topics) = baseline();
    topics.roles.push(topics.roles[0].clone());
    assert!(encode_snapshot(guild_id, 2, now, &topics).is_none());
    topics.roles.pop();
    topics.members.push(topics.members[0].clone());
    assert!(encode_snapshot(guild_id, 2, now, &topics).is_none());
    topics.members.pop();
    topics.voice.push(topics.voice[0].clone());
    assert!(encode_snapshot(guild_id, 2, now, &topics).is_none());
    topics.voice.pop();
    topics.events.push(topics.events[0].clone());
    assert!(encode_snapshot(guild_id, 2, now, &topics).is_none());
}

#[test]
fn requires_permission_and_room_dependencies_before_encoding() {
    let (guild_id, now, mut topics) = baseline();
    assert!(encode_snapshot(id(999), 2, now, &topics).is_none());
    let owner = topics.guild.owner_id;
    topics.guild.owner_id = id(999);
    assert!(encode_snapshot(guild_id, 2, now, &topics).is_none());
    topics.guild.owner_id = owner;

    topics.channels[0].overwrites.push(Overwrite {
        kind: OverwriteKind::Role,
        id: id(999),
        allow: Permissions(1024),
        deny: Permissions(0),
    });
    assert!(encode_snapshot(guild_id, 2, now, &topics).is_none());
    topics.channels[0].overwrites[0].id = guild_id;
    assert!(encode_snapshot(guild_id, 2, now, &topics).is_some());

    topics.voice[0].user_id = id(999);
    assert!(encode_snapshot(guild_id, 2, now, &topics).is_none());
    topics.voice[0].user_id = owner;
    topics.events[0].channel_id = Some(id(999));
    assert!(encode_snapshot(guild_id, 2, now, &topics).is_none());
    topics.events[0].channel_id = Some(topics.channels[0].id);
    topics.channels[0].kind = ChannelKind::Stage;
    assert!(encode_snapshot(guild_id, 2, now, &topics).is_some());
    topics.channels[0].kind = ChannelKind::Other;
    let occupant = topics.voice.pop().unwrap();
    assert!(encode_snapshot(guild_id, 2, now, &topics).is_none());
    topics.events.clear();
    assert!(encode_snapshot(guild_id, 2, now, &topics).is_some());
    topics.voice.push(occupant);
    assert!(encode_snapshot(guild_id, 2, now, &topics).is_none());
}
