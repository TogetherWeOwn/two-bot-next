//! Registration is part of the command contract: Discord must let either time
//! form through, while the runtime enforces their exclusive choice.

use two_bot_core::commands::CommandOptionType;
use two_bot_core::router::{InteractionRouter, RouterGates};

#[test]
fn schedule_picker_allows_absolute_or_relative_time_without_weakening_authority() {
    let commands = InteractionRouter::new(RouterGates {
        configured_guild: None,
        scorecard: false,
        automations: true,
        announcements: false,
        moderation: false,
        voice: false,
        voice_assistant: false,
        tickets: false,
        self_roles: false,
        onboarding_picker: false,
        session_picker: false,
    })
    .publish_set(&[])
    .expect("compiled registry");
    let schedule = commands
        .iter()
        .find(|command| command.name == "schedule")
        .expect("automation schedule command");
    assert!(!schedule.dm_permission);
    assert_eq!(schedule.default_member_permissions.as_deref(), Some("32"));
    let at = schedule
        .options
        .iter()
        .find(|option| option.name == "at")
        .expect("publish the RFC3339 at option so Discord can send it");
    assert_eq!(at.kind, CommandOptionType::String.as_u8());
    assert!(!at.required.unwrap_or(false), "relative remains usable");
    let relative = schedule
        .options
        .iter()
        .find(|option| option.name == "in-minutes")
        .expect("retain relative schedules");
    assert!(
        !relative.required.unwrap_or(false),
        "requiring in-minutes prevents an at-only invocation in Discord"
    );
    assert_eq!(relative.min_value, Some(1));
    assert_eq!(relative.max_value, Some(525_600));
    let recurring = schedule
        .options
        .iter()
        .find(|option| option.name == "every-minutes")
        .expect("retain interval schedules");
    assert!(!recurring.required.unwrap_or(false));
    let body = schedule
        .options
        .iter()
        .find(|option| option.name == "body")
        .expect("body option");
    assert!(body.required.unwrap_or(false));
    assert_eq!(body.max_length, Some(2_000));
}
