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
    for base in [Permissions::CREATE_EVENTS, Permissions::ADMINISTRATOR] {
        let cache = SharedCache::default();
        ready(&cache);
        let mut guild = guild();
        guild["roles"][0]["permissions"] = json!(base.bits().to_string());
        guild["channels"][0]["permission_overwrites"] = json!([{
            "id":"999","type":1,"allow":"0","deny":Permissions::CREATE_EVENTS.bits().to_string()
        }]);
        seed(&cache, guild);
        let evidence = cache
            .event_permission_evidence(Id::new(100), Id::new(999))
            .expect("complete fixture");
        let admin = base == Permissions::ADMINISTRATOR;
        assert_eq!(evidence.owner_or_admin, admin);
        assert!(evidence.guild_create_events);
        assert_eq!(evidence.guild_manage_events, admin);
        assert_eq!(evidence.voice.channels, 1);
        assert_eq!(evidence.voice.either_event_permission, usize::from(admin));
    }
}

#[test]
fn permission_projection_refuses_partial_or_unrelated_cache() {
    for fault in [
        "wrong identity",
        "missing role",
        "missing overwrites",
        "stale session",
    ] {
        let cache = SharedCache::default();
        ready(&cache);
        let mut guild = guild();
        if fault == "missing role" {
            guild["members"][0]["roles"] = json!(["101"]);
        }
        if fault == "missing overwrites" {
            guild["channels"][0]["permission_overwrites"] = Value::Null;
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
