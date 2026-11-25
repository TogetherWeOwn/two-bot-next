use super::*;
use serde_json::{json, Value};
use twilight_model::gateway::{event::Event, payload::incoming::GuildCreate};

fn ready(cache: &SharedCache) {
    cache.update(&Event::Ready(
        serde_json::from_value(json!({
            "v":10, "session_id":"fixture", "resume_gateway_url":"ws://fixture.invalid",
            "application":{"id":"999","flags":0}, "guilds":[],
            "user":{"id":"999","bot":true,"username":"fixture","discriminator":"0","mfa_enabled":false}
        }))
        .expect("ready fixture"),
    ));
}

fn guild() -> Value {
    serde_json::from_str(include_str!("../../../bot/tests/fixtures/voice_guild.json"))
        .expect("synthetic gateway fixture")
}

fn seed(cache: &SharedCache, guild: Value) {
    cache.update(&Event::GuildCreate(Box::new(GuildCreate::Available(
        serde_json::from_value(guild).expect("synthetic guild frame"),
    ))));
}

#[test]
fn permission_projection_resolves_event_bits_and_bypass() {
    for (owner, base) in [
        (false, Permissions::CREATE_EVENTS),
        (false, Permissions::ADMINISTRATOR),
        (true, Permissions::CREATE_EVENTS),
    ] {
        let cache = SharedCache::default();
        ready(&cache);
        let mut guild = guild();
        if owner {
            guild["owner_id"] = json!("999");
        }
        guild["roles"][0]["permissions"] = json!(base.bits().to_string());
        guild["channels"][0]["permission_overwrites"] = json!([{
            "id":"999","type":1,"allow":"0","deny":Permissions::CREATE_EVENTS.bits().to_string()
        }]);
        guild["channels"]
            .as_array_mut()
            .expect("channels")
            .push(json!({"id":"300","type":15,"name":"forum fixture"}));
        guild["threads"] = Value::Array(
            (1_000..3_049)
                .map(|id: u64| {
                    json!({
                        "id":id.to_string(),"type":11,"parent_id":"300",
                        "thread_metadata":{"archived":true,"auto_archive_duration":1440,
                            "archive_timestamp":"2026-09-30T00:00:00+00:00","locked":false}
                    })
                })
                .collect(),
        );
        seed(&cache, guild);
        let evidence = cache
            .event_permission_evidence(Id::new(100), Id::new(999))
            .expect("complete fixture");
        let bypass = owner || base == Permissions::ADMINISTRATOR;
        assert_eq!(evidence.owner_or_admin, bypass);
        assert!(evidence.guild_create_events);
        assert_eq!(evidence.guild_manage_events, bypass);
        assert_eq!(evidence.voice.channels, 1);
        assert_eq!(evidence.voice.either_event_permission, usize::from(bypass));
    }
}

#[test]
fn permission_projection_refuses_partial_or_unrelated_cache() {
    for fault in [
        "wrong identity",
        "missing role",
        "missing overwrites",
        "stale session",
        "active timeout",
        "duplicate overwrite",
    ] {
        let cache = SharedCache::default();
        ready(&cache);
        let mut guild = guild();
        match fault {
            "missing role" => guild["members"][0]["roles"] = json!(["101"]),
            "missing overwrites" => guild["channels"][0]["permission_overwrites"] = Value::Null,
            "active timeout" => {
                guild["members"][0]["communication_disabled_until"] = json!("2099-01-01T00:00:00Z");
            }
            "duplicate overwrite" => {
                let entry = json!({"id":"100","type":0,"allow":"0","deny":"0"});
                guild["channels"][0]["permission_overwrites"] = json!([entry, entry]);
            }
            _ => {}
        }
        seed(&cache, guild);
        if fault == "stale session" {
            ready(&cache);
        }
        let bot = if fault == "wrong identity" { 998 } else { 999 };
        assert!(
            cache
                .event_permission_evidence(Id::new(100), Id::new(bot))
                .is_none(),
            "{fault}"
        );
    }
}

#[test]
fn obfuscated_event_channels_refuse_placeholder_overwrites() {
    for kind in [2, 13] {
        let cache = SharedCache::default();
        ready(&cache);
        let mut guild = guild();
        guild["channels"][0]["type"] = json!(kind);
        guild["channels"][0]["flags"] = json!(ChannelFlags::CHANNEL_OBFUSCATED.bits());
        guild["channels"][0]["permission_overwrites"] = json!([{
            "id":"100","type":0,"allow":"0","deny":Permissions::VIEW_CHANNEL.bits().to_string()
        }]);
        seed(&cache, guild);
        assert!(cache
            .event_permission_evidence(Id::new(100), Id::new(999))
            .is_none());
    }
}
