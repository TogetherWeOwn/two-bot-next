use std::collections::HashSet;

use super::{
    decode, event_change, wire, Change, Deserialize, Dispatch, EntityType, GuildMarker, GuildScope,
    Id, Kind, PrivacyLevel, ProjectionError, RawValue, ScheduledEventInput,
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
        let input = decode(body, '{')?;
        validate_baseline_row(&input)?;
        let (id, event) = match event_change(input)? {
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

// Validate the full row before projection can discard terminal events or
// fields that are not part of the wire shape. Leave delta semantics unchanged.
fn validate_baseline_row(input: &ScheduledEventInput) -> Result<(), ProjectionError> {
    let valid_entity = match (&input.entity_type, input.channel_id, &input.entity_metadata) {
        (EntityType::StageInstance | EntityType::Voice, Some(_), None) => true,
        (EntityType::External, None, Some(metadata)) => {
            metadata.location.is_some() && input.scheduled_end_time.is_some()
        }
        _ => false,
    };
    if input.privacy_level != PrivacyLevel::GuildOnly || !valid_entity {
        return Err(ProjectionError::UnsupportedEvent);
    }
    Ok(())
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
        let mut voice = event("95", 1);
        voice["entity_type"] = json!(2);
        voice["channel_id"] = json!("30");
        voice["entity_metadata"] = Value::Null;
        voice["scheduled_end_time"] = Value::Null;
        let raw = dispatch(json!({
            "id": "10",
            "guild_scheduled_events": [event("91", 1), event("92", 2), event("93", 3), event("94", 4), voice]
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
                 "location": "Venue", "startsAt": "2026-11-21T13:00:00Z", "endsAt": "2026-11-21T15:00:00Z"},
                {"id": "95", "channelId": "30", "status": "scheduled", "title": "Tournament", "description": null,
                 "location": null, "startsAt": "2026-11-21T13:00:00Z", "endsAt": null}
            ])
        );
    }

    #[test]
    fn inconsistent_entity_fields_or_privacy_invalidate_the_whole_baseline() {
        for (case, status, overrides) in [
            (
                "cancelled external with channel",
                4,
                json!({"channel_id": "30"}),
            ),
            (
                "completed without guild-only privacy",
                3,
                json!({"privacy_level": 99}),
            ),
            (
                "external without metadata",
                2,
                json!({"entity_metadata": null}),
            ),
            (
                "external without location",
                2,
                json!({"entity_metadata": {"location": null}}),
            ),
            (
                "external without end time",
                2,
                json!({"scheduled_end_time": null}),
            ),
            (
                "stage with metadata",
                2,
                json!({"entity_type": 1, "channel_id": "30"}),
            ),
            (
                "voice without channel",
                2,
                json!({"entity_type": 2, "entity_metadata": null}),
            ),
        ] {
            let mut row = event("92", status);
            for (key, value) in overrides.as_object().expect("fixture overrides") {
                row[key.as_str()] = value.clone();
            }
            let raw =
                dispatch(json!({"id": "10", "guild_scheduled_events": [event("91", 1), row]}));
            assert!(
                matches!(
                    scheduled_events(GUILD, &raw),
                    Err(ProjectionError::UnsupportedEvent)
                ),
                "{case}"
            );
        }
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
