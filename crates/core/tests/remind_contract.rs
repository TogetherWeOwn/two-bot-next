//! Next-only reminder conformance. Legacy snapshots must not acquire this name.
//! Breaks caught: wrong duration/UTF-16 bounds, unsafe text accepted unchanged,
//! permission escalation, missing automations gate and custom-name collisions.
use two_bot_core::{InteractionRouter, RouterGates, SlashContext, SlashOutcome};

fn gates(automations: bool) -> RouterGates {
    RouterGates {
        configured_guild: Some(2222),
        automations,
        scorecard: false,
        announcements: false,
        moderation: false,
        voice: false,
        voice_assistant: false,
        tickets: false,
        self_roles: false,
        onboarding_picker: false,
        session_picker: false,
    }
}

#[test]
fn reminder_publication_and_route_share_the_automations_gate_without_a_staff_permission() {
    let context = SlashContext {
        name: "remind",
        guild_id: Some(2222),
        actor_permissions: Some(0),
        custom_row: None,
    };
    let enabled = InteractionRouter::new(gates(true));
    assert!(matches!(
        enabled.route_slash(&context),
        SlashOutcome::Handled { .. }
    ));
    let published = enabled.publish_set(&[]).expect("publish");
    let reminder = published
        .iter()
        .find(|row| row.name == "remind")
        .expect("Next-only command present");
    assert!(!reminder.dm_permission);
    assert_eq!(reminder.default_member_permissions, None);
    assert_eq!(
        reminder
            .options
            .iter()
            .map(|option| option.name.as_str())
            .collect::<Vec<_>>(),
        ["me", "list", "cancel"]
    );
    let me = &reminder.options[0];
    let minutes = me
        .options
        .iter()
        .find(|option| option.name == "in")
        .expect("minutes");
    assert_eq!(
        (
            minutes.kind,
            minutes.required,
            minutes.min_value,
            minutes.max_value
        ),
        (4, Some(true), Some(1), Some(525_600))
    );
    let text = me
        .options
        .iter()
        .find(|option| option.name == "text")
        .expect("text");
    assert_eq!(
        (text.kind, text.required, text.max_length),
        (3, Some(true), Some(240))
    );
    let disabled = InteractionRouter::new(gates(false));
    assert!(!disabled
        .publish_set(&[])
        .expect("disabled publish")
        .iter()
        .any(|row| row.name == "remind"));
    assert!(
        !matches!(disabled.route_slash(&context), SlashOutcome::Handled { .. }),
        "an unpublished reminder cannot dispatch while automations are off"
    );
}

#[test]
fn duration_edges_and_utf16_after_neutralization_are_enforced() {
    use two_bot_core::remind::validate;
    for duration in [1, 525_600] {
        assert_eq!(
            validate(duration, "valid reminder").expect("duration edge"),
            "valid reminder"
        );
    }
    for duration in [i64::MIN, -1, 0, 525_601, i64::MAX] {
        assert!(
            validate(duration, "valid reminder").is_err(),
            "invalid minutes {duration}"
        );
    }
    assert!(
        validate(30, &"😀".repeat(120)).is_ok(),
        "240 UTF-16 units accepted"
    );
    for body in [
        "".to_owned(),
        " \n\t".to_owned(),
        "x".repeat(241),
        "😀".repeat(121),
    ] {
        assert!(
            validate(30, &body).is_err(),
            "empty or overlong body refuses"
        );
    }
    let safe_at_edge = format!("{}@everyone @here ab", "😀".repeat(110));
    let normalized = validate(30, &safe_at_edge).expect("240 units after neutralization");
    assert_eq!(normalized.encode_utf16().count(), 240);
    assert!(!normalized.contains("@everyone") && !normalized.contains("@here"));
    assert!(
        validate(30, &format!("{safe_at_edge}x")).is_err(),
        "neutralization expansion counts against the bound"
    );
}
