//! Protected discovery contract for the read-only /myroom voice command.
//! Keep this file unchanged in the paired implementation. Existing staging
//! manifest/reference-doc tests own byte-for-byte compatibility pin updates.

use two_bot_core::{help_text, InteractionRouter, RouterGates, SlashContext, SlashOutcome};

const GUILD: u64 = 2222;

fn gates(voice: bool, assistant: bool) -> RouterGates {
    RouterGates {
        configured_guild: Some(GUILD),
        scorecard: false,
        automations: false,
        announcements: false,
        moderation: false,
        voice,
        voice_assistant: assistant,
        tickets: false,
        self_roles: false,
        onboarding_picker: false,
        session_picker: false,
    }
}

#[test]
fn myroom_publishes_once_for_ordinary_members_only_with_voice_enabled() {
    // Break caught: a missing definition, an admin-only picker, a targeted
    // member parameter, or accidental coupling to a non-voice feature gate.
    for voice in [false, true] {
        for assistant in [false, true] {
            let router = InteractionRouter::new(gates(voice, assistant));
            let published = router.publish_set(&[]).expect("valid compiled publication");
            let definitions: Vec<_> = published
                .iter()
                .filter(|def| def.name == "myroom")
                .collect();
            assert_eq!(
                definitions.len(),
                usize::from(voice),
                "voice={voice}, assistant={assistant}"
            );
            if let Some(definition) = definitions.first() {
                assert!(!definition.description.is_empty());
                assert!(!definition.dm_permission, "rooms are guild-local");
                assert_eq!(
                    definition.default_member_permissions, None,
                    "ordinary member read"
                );
                assert!(
                    definition.options.is_empty(),
                    "caller identity is authenticated, never an option"
                );
            }
        }
    }
}

#[test]
fn myroom_shared_router_yields_to_the_voice_sink_even_with_a_same_named_custom_row() {
    // Break caught: published-but-unclaimed command, admin permission check,
    // or a custom command taking precedence over the private room read.
    let router = InteractionRouter::new(gates(true, false));
    assert!(router.voice_owns_command("myroom"));
    for permissions in [None, Some(0), Some(u64::MAX)] {
        for custom_row in [None, Some(true), Some(false)] {
            let context = SlashContext {
                name: "myroom",
                guild_id: Some(GUILD),
                actor_permissions: permissions,
                custom_row,
            };
            assert_eq!(router.route_slash(&context), SlashOutcome::Ignore);
        }
    }
}

#[test]
fn myroom_help_is_discoverable_with_the_member_voice_commands() {
    let router = InteractionRouter::new(gates(true, false));
    let text = help_text(&router.publish_set(&[]).expect("voice publication"));
    let voice_section = text
        .split("**Voice rooms")
        .nth(1)
        .and_then(|section| section.split("\n**").next())
        .expect("voice help group");
    assert!(
        voice_section.contains("/myroom"),
        "missing member discovery: {text}"
    );
    let off = InteractionRouter::new(gates(false, false));
    assert!(!help_text(&off.publish_set(&[]).unwrap()).contains("/myroom"));
}

#[test]
fn control_voice_off_does_not_claim_or_advertise_myroom() {
    let router = InteractionRouter::new(gates(false, false));
    assert!(!router.voice_owns_command("myroom"));
    assert_eq!(
        router.route_slash(&SlashContext {
            name: "myroom",
            guild_id: Some(GUILD),
            actor_permissions: Some(0),
            custom_row: None,
        }),
        SlashOutcome::Unknown
    );
}
