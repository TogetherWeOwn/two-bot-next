use std::collections::HashSet;

use super::{
    decode, event_change, wire, Change, Deserialize, Dispatch, GuildMarker, GuildScope, Id, Kind,
    ProjectionError, RawValue,
};

#[derive(Deserialize)]
struct GuildInput<'a> {
    id: Id<GuildMarker>,
    #[serde(default)]
    unavailable: bool,
    #[serde(borrow)]
    guild_scheduled_events: Option<&'a RawValue>,
}

/// Read the events topic from an original `GUILD_CREATE` dispatch, before
/// Twilight can default an omitted list to empty. `Some(empty)` proves there
/// are no scheduled or active events; `None` supplies no baseline. An error
/// means no event rows can be used from this dispatch.
///
/// The caller owns gateway generation and receive order, and must run full
/// snapshot validation (including channel references) before publication.
pub fn scheduled_events(
    guild_id: Id<GuildMarker>,
    raw: &str,
) -> Result<Option<Vec<wire::Event>>, ProjectionError> {
    let dispatch: Dispatch<'_> = decode(raw, '{')?;
    if !matches!(dispatch.kind()?, Some(Kind::GuildCreate)) {
        return Ok(None);
    }
    let body = dispatch.d.ok_or(ProjectionError::MalformedDispatch)?;
    let guild: GuildInput<'_> = decode(body.get(), '{')?;
    if guild.id != guild_id || guild.unavailable {
        return Ok(None);
    }
    let list = guild
        .guild_scheduled_events
        .ok_or(ProjectionError::MalformedDispatch)?;
    let rows: Vec<&RawValue> = decode(list.get(), '[')?;
    let mut seen = HashSet::new();
    let mut events = Vec::new();
    for row in rows {
        let body = row.get();
        if decode::<GuildScope>(body, '{')?.guild_id != guild_id {
            return Err(ProjectionError::MalformedDispatch);
        }
        let change = event_change(decode(body, '{')?)?;
        let (id, event) = match change {
            Change::Upsert { id, value } => (id, Some(value)),
            Change::Remove { id } => (id, None),
        };
        if !seen.insert(id) {
            return Err(ProjectionError::MalformedDispatch);
        }
        events.extend(event);
    }
    Ok(Some(events))
}

#[cfg(test)]
mod tests {
    use serde_json::{json, Value};
    use twilight_model::guild::Guild;

    use super::*;

    const GUILD: Id<GuildMarker> = Id::new(10);

    fn dispatch(body: Value) -> String {
        json!({"op": 0, "s": 4, "t": "GUILD_CREATE", "d": body}).to_string()
    }

    fn event(id: &str, status: u8) -> Value {
        json!({
            "id": id, "guild_id": "10", "entity_type": 3, "privacy_level": 2, "status": status,
            "channel_id": null, "name": "Tournament", "entity_metadata": {"location": "Venue"},
            "scheduled_start_time": "2026-11-21T15:00:00+02:00",
            "scheduled_end_time": "2026-11-21T17:00:00+02:00", "user_count": 7
        })
    }

    #[test]
    fn real_twilight_model_cannot_distinguish_omitted_from_explicitly_empty() {
        let mut guild = json!({
            "id": "10", "name": "Fixture", "owner_id": "99", "afk_timeout": 300,
            "default_message_notifications": 0, "explicit_content_filter": 0, "features": [],
            "mfa_level": 0, "nsfw_level": 0, "preferred_locale": "en-US", "roles": [],
            "premium_progress_bar_enabled": false, "system_channel_flags": 0, "verification_level": 0
        });
        let omitted: Guild = serde_json::from_value(guild.clone()).expect("available guild");
        assert!(omitted.guild_scheduled_events.is_empty());
        assert!(matches!(
            scheduled_events(GUILD, &dispatch(guild.clone())),
            Err(ProjectionError::MalformedDispatch)
        ));
        guild["guild_scheduled_events"] = json!([]);
        let empty: Guild = serde_json::from_value(guild.clone()).expect("available guild");
        assert!(empty.guild_scheduled_events.is_empty());
        let events = scheduled_events(GUILD, &dispatch(guild))
            .expect("valid dispatch")
            .expect("explicitly complete events");
        assert!(events.is_empty());
    }

    #[test]
    fn only_an_available_guild_create_for_the_configured_guild_supplies_the_list() {
        for raw in [
            dispatch(json!({"id": "11", "guild_scheduled_events": []})),
            dispatch(json!({"id": "10", "unavailable": true, "guild_scheduled_events": []})),
            r#"{"op":0,"t":"GUILD_UPDATE","d":{"id":"10","guild_scheduled_events":[]}}"#.into(),
            r#"{"op":11,"t":"GUILD_CREATE","d":{"id":"10","guild_scheduled_events":[]}}"#.into(),
        ] {
            assert!(matches!(scheduled_events(GUILD, &raw), Ok(None)));
        }
    }

    #[test]
    fn explicit_list_projects_only_current_allowlisted_event_fields() {
        let raw = dispatch(json!({
            "id": "10", "guild_scheduled_events": [event("91", 1), event("92", 2), event("93", 3), event("94", 4)]
        }));
        let rows = scheduled_events(GUILD, &raw)
            .expect("valid dispatch")
            .expect("explicitly complete events");
        assert_eq!(
            serde_json::to_value(rows).expect("wire rows"),
            json!([
                {"id": "91", "channelId": null, "status": "scheduled", "title": "Tournament", "description": null,
                 "location": "Venue", "startsAt": "2026-11-21T13:00:00Z", "endsAt": "2026-11-21T15:00:00Z"},
                {"id": "92", "channelId": null, "status": "active", "title": "Tournament", "description": null,
                 "location": "Venue", "startsAt": "2026-11-21T13:00:00Z", "endsAt": "2026-11-21T15:00:00Z"}
            ])
        );
    }

    #[test]
    fn malformed_lists_or_any_bad_row_invalidate_the_whole_baseline() {
        let mut foreign = event("92", 1);
        foreign["guild_id"] = json!("11");
        let mut malformed_metadata = event("92", 1);
        malformed_metadata["entity_metadata"] = json!(["Venue"]);
        let mut malformed_terminal = event("92", 3);
        malformed_terminal["scheduled_start_time"] = json!("invalid timestamp");
        for invalid in [
            Value::Null,
            json!({}),
            json!([event("91", 1), ["10"]]),
            json!([event("91", 1), malformed_metadata]),
            json!([event("91", 1), malformed_terminal]),
            json!([event("91", 1), foreign]),
            json!([event("91", 1), event("91", 4)]),
        ] {
            assert!(matches!(
                scheduled_events(
                    GUILD,
                    &dispatch(json!({"id":"10", "guild_scheduled_events": invalid}))
                ),
                Err(ProjectionError::MalformedDispatch)
            ));
        }
        for raw in [
            r#"{"op":0,"t":"GUILD_CREATE","d":{"id":"10","guild_scheduled_events":[],"guild_scheduled_events":[]}}"#,
            r#"{"op":0,"t":"GUILD_CREATE","d":["10",false,[]]}"#,
            r#"[0,"GUILD_CREATE",{"id":"10","guild_scheduled_events":[]}]"#,
        ] {
            assert!(matches!(
                scheduled_events(GUILD, raw),
                Err(ProjectionError::MalformedDispatch)
            ));
        }
        let raw = dispatch(
            json!({"id":"10", "guild_scheduled_events": [event("91", 1), event("92", 99)]}),
        );
        assert!(matches!(
            scheduled_events(GUILD, &raw),
            Err(ProjectionError::UnsupportedEvent)
        ));
    }
}
