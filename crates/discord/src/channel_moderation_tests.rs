use super::{
    register_channel_handlers, request, CallFailure, ChannelModerationRuntime, ChannelReply,
    DiscordError, Interaction, InteractionRouter, ModerationAction,
};
use crate::ActionExecutor;
use serde_json::json;
use two_bot_core::{ChannelModerationStore, RouterGates};

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

#[test]
fn uncertain_reply_pins_no_new_mutation_wording() {
    let reply = ChannelReply::uncertain();
    assert_eq!(
        reply.text,
        "This channel action is in progress or requires reconciliation; no new mutation was attempted."
    );
    assert_eq!(reply.outcome, "in_progress");
    assert!(!reply.replayed);
}

#[test]
fn safe_pre_mutation_refusal_pins_no_mutation_accepted_wording() {
    let failure = CallFailure::of(DiscordError::Rejected("synthetic rejection".to_owned()));
    assert!(failure.error.is_safe_pre_mutation());
    assert!(failure.no_effect);
    let reply = failure.refusal();
    assert_eq!(
        reply.text,
        "Discord refused the channel action; no mutation was accepted."
    );
    assert_eq!(reply.outcome, "refused");
    assert!(!reply.replayed);
}

#[test]
fn retryable_refusal_pins_no_mutation_accepted_wording() {
    // A 429 proves no effect but is not a safe-pre-mutation error, so it
    // reaches the other refusal branch without claiming an uncertain write is safe.
    let failure = CallFailure::of(DiscordError::RateLimited);
    assert!(!failure.error.is_safe_pre_mutation());
    assert!(failure.no_effect);
    let reply = failure.refusal();
    assert_eq!(
        reply.text,
        "Discord could not complete the channel action right now; no mutation was accepted. Try again shortly."
    );
    assert_eq!(reply.outcome, "refused");
    assert!(!reply.replayed);
}

#[tokio::test]
async fn missing_context_reply_pins_required_guild_member_and_channel_wording() {
    // A closed lazy pool makes any unexpected SQL fail, without a database.
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://agent_test@127.0.0.1:1/agent_test")
        .expect("lazy test pool");
    pool.close().await;
    let runtime = ChannelModerationRuntime::new(
        ChannelModerationStore::from_pool(pool),
        ActionExecutor::with_proxy(
            "synthetic-test-token".to_owned(),
            Some("http://127.0.0.1:1".to_owned()),
        )
        .expect("synthetic executor"),
    );
    let mut router = InteractionRouter::new(RouterGates {
        configured_guild: Some(2222),
        moderation: true,
        scorecard: false,
        automations: false,
        announcements: false,
        voice: false,
        voice_assistant: false,
        tickets: false,
        self_roles: false,
        onboarding_picker: false,
        session_picker: false,
    });
    register_channel_handlers(&mut router);
    for action in ["purge", "slowmode", "lockdown", "unlock"] {
        for missing in ["guild", "member", "member_user", "channel"] {
            let mut interaction = interaction(Some("3333"), None);
            let Some(
                twilight_model::application::interaction::InteractionData::ApplicationCommand(data),
            ) = interaction.data.as_mut()
            else {
                panic!("synthetic slash command");
            };
            data.name = action.to_owned();
            match missing {
                "guild" => interaction.guild_id = None,
                "member" => interaction.member = None,
                "member_user" => interaction.member.as_mut().expect("fixture member").user = None,
                "channel" => interaction.channel = None,
                _ => unreachable!(),
            }
            let reply = runtime
                .execute(&router, &interaction)
                .await
                .expect("missing context must not use the closed pool")
                .expect("channel command reply");
            assert_eq!(
                reply.text, "A guild member and channel are required.",
                "{action}: missing {missing}"
            );
            assert_eq!(reply.outcome, "refused");
            assert!(!reply.replayed);
        }
    }
}
