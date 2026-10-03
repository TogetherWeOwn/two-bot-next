use super::{request, Interaction, ModerationAction};
use serde_json::json;

fn interaction(channel: Option<&str>, channel_id: Option<&str>) -> Interaction {
    serde_json::from_value(json!({
        "id": "7", "application_id": "1111", "type": 2,
        "guild_id": "2222",
        "channel": channel.map(|id| json!({"id": id, "type": 0})),
        "channel_id": channel_id,
        "data": {"id": "1", "type": 1, "name": "slowmode", "options": [
            {"name": "seconds", "type": 4, "value": 0},
            {"name": "reason", "type": 3, "value": "channel decoding test"}
        ]},
        "member": {"user": {"id": "88", "username": "test", "discriminator": "0"},
            "roles": [], "flags": 0, "deaf": false, "mute": false, "permissions": "16"},
        "token": "synthetic-test-token", "version": 1,
        "authorizing_integration_owners": {"0": "2222"}, "entitlements": []
    }))
    .expect("synthetic Twilight interaction")
}

#[test]
fn legacy_invoking_channel_matches_modern_request_and_hash() {
    let modern = request(&interaction(Some("3333"), None), ModerationAction::Slowmode).unwrap();
    let legacy = request(&interaction(None, Some("3333")), ModerationAction::Slowmode).unwrap();
    assert_eq!(legacy.row.channel_id.as_deref(), Some("3333"));
    assert_eq!(legacy.hash, modern.hash);
    assert_eq!(legacy.numeric, Some(0));
    assert!(legacy.validation.is_ok());
}

#[test]
fn modern_invoking_channel_takes_precedence_over_legacy_field() {
    let modern = request(&interaction(Some("3333"), None), ModerationAction::Slowmode).unwrap();
    let both = request(
        &interaction(Some("3333"), Some("9999")),
        ModerationAction::Slowmode,
    )
    .unwrap();
    assert_eq!(both.row.channel_id.as_deref(), Some("3333"));
    assert_eq!(both.hash, modern.hash);
}

#[test]
fn missing_invoking_channel_cannot_form_a_moderation_request() {
    assert!(request(&interaction(None, None), ModerationAction::Slowmode).is_none());
}
