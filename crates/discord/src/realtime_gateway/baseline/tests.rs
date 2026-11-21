#![cfg(test)]

use serde_json::{json, Value};

use super::*;

const GUILD: Id<GuildMarker> = Id::new(10);

fn dispatch(body: &Value) -> String {
    json!({"op": 0, "t": "GUILD_CREATE", "s": 4, "d": body}).to_string()
}

fn empty_guild() -> Value {
    json!({
        "id": "10", "owner_id": "99", "afk_channel_id": null, "channels": [],
        "roles": [{"id": "10", "permissions": "1024"}], "voice_states": []
    })
}

fn room() -> Value {
    json!({"id":"12", "type":2, "name":"Lounge", "position":0, "permission_overwrites":[]})
}

fn assert_invalid(guild: &Value) {
    assert!(topology(GUILD, &dispatch(guild)).is_err(), "{guild}");
}

#[test]
fn explicit_empty_lists_are_distinct_from_absent_or_malformed_lists() {
    let guild = empty_guild();
    let rows = topology(GUILD, &dispatch(&guild))
        .expect("valid envelope")
        .expect("complete topology");
    assert!(rows.channels.is_empty());
    assert!(rows.voice.is_empty());
    assert_eq!(rows.roles.len(), 1);
    for key in [
        "owner_id",
        "afk_channel_id",
        "channels",
        "roles",
        "voice_states",
    ] {
        let mut absent = guild.clone();
        absent.as_object_mut().expect("guild object").remove(key);
        assert_invalid(&absent);
    }
    for key in ["channels", "roles", "voice_states"] {
        for value in [Value::Null, json!({})] {
            let mut invalid = guild.clone();
            invalid[key] = value;
            assert_invalid(&invalid);
        }
    }
}

#[test]
fn projects_only_allowlisted_fields_and_preserves_raw_permission_masks() {
    let mut guild = empty_guild();
    let high_bit = "9223372036854775808";
    guild["afk_channel_id"] = json!("12");
    guild["channels"] = json!([{
        "id":"12", "type":2, "guild_id":"10", "name":"Lounge", "position":0, "topic":"unprojected",
        "permission_overwrites":[{"type":0,"id":"10","allow":high_bit,"deny":"0"}]
    }]);
    guild["roles"] = json!([{"id":"10", "permissions":high_bit,"name":"unprojected"}]);
    guild["voice_states"] = json!([{
        "user_id":"99","channel_id":"12","session_id":"unprojected"
    }]);
    let rows = topology(GUILD, &dispatch(&guild))
        .expect("valid envelope")
        .expect("complete topology");
    assert_eq!(
        json!([rows.guild, rows.channels, rows.roles, rows.voice]),
        json!([
            {"ownerId":"99","afkChannelId":"12"},
            [{"id":"12","name":"Lounge","kind":"voice","parentId":null,"position":0,
              "overwrites":[{"kind":"role","id":"10","allow":high_bit,"deny":"0"}]}],
            [{"id":"10","permissions":high_bit}],
            [{"userId":"99","channelId":"12"}]
        ])
    );
}

#[test]
fn only_an_available_create_for_the_configured_guild_proves_topology() {
    for raw in [
        dispatch(&json!({"id":"11"})),
        dispatch(&json!({"id":"10","unavailable":true})),
        r#"{"op":0,"t":"GUILD_UPDATE","d":{"id":"10"}}"#.into(),
        r#"{"op":11,"t":"GUILD_CREATE"}"#.into(),
    ] {
        assert!(matches!(topology(GUILD, &raw), Ok(None)));
    }
}

#[test]
fn duplicates_foreign_rows_and_incomplete_permission_data_invalidate_the_baseline() {
    let overwrite = |id: u64| json!({"id":id.to_string(),"type":0,"allow":"0","deny":"0"});
    for overwrites in [
        json!([["10", 0, "0", "0"]]),
        json!([overwrite(10), overwrite(10)]),
        Value::Array((10..111).map(overwrite).collect()),
    ] {
        let mut guild = empty_guild();
        let mut channel = room();
        channel["permission_overwrites"] = overwrites;
        guild["channels"] = json!([channel]);
        assert_invalid(&guild);
    }
    let mut foreign = room();
    foreign["guild_id"] = json!("11");
    for (key, value) in [
        ("channels", json!([room(), room()])),
        ("channels", json!([{"id":"12","type":2,"name":"Lounge"}])),
        ("channels", json!([{"id":"12","type":11}])),
        ("channels", json!([foreign])),
        ("roles", json!([])),
        (
            "roles",
            json!([{"id":"10","permissions":"0"},{"id":"10","permissions":"0"}]),
        ),
        (
            "roles",
            json!([{"id":"10","permissions":"18446744073709551616"}]),
        ),
        ("voice_states", json!([{"user_id":"99","channel_id":null}])),
        (
            "voice_states",
            json!([{"user_id":"99","channel_id":"12","guild_id":"11"}]),
        ),
        (
            "voice_states",
            json!([{"user_id":"99","channel_id":"12"},{"user_id":"99","channel_id":"13"}]),
        ),
    ] {
        let mut guild = empty_guild();
        guild[key] = value;
        assert_invalid(&guild);
    }
    let raw =
        dispatch(&empty_guild()).replace("\"channels\":[]", "\"channels\":[],\"channels\":[]");
    assert!(topology(GUILD, &raw).is_err());
    assert!(topology(GUILD, r#"[0,"GUILD_CREATE",{"id":"10"}]"#).is_err());
}

#[test]
fn topology_rejects_collections_beyond_the_v1_snapshot_limits() {
    for (key, cap) in [("channels", 500), ("roles", 250), ("voice_states", 10_000)] {
        let row = |index: usize| {
            let id = (10 + index).to_string();
            match key {
                "channels" => {
                    let mut channel = room();
                    channel["id"] = json!(id);
                    channel
                }
                "roles" => json!({"id":id,"permissions":"0"}),
                _ => json!({"user_id":id,"channel_id":"12"}),
            }
        };
        let mut guild = empty_guild();
        guild[key] = Value::Array((0..cap).map(row).collect());
        assert!(matches!(topology(GUILD, &dispatch(&guild)), Ok(Some(_))));
        guild[key].as_array_mut().expect("rows").push(row(cap));
        assert!(matches!(
            topology(GUILD, &dispatch(&guild)),
            Err(ProjectionError::InvalidPayload)
        ));
    }
}
