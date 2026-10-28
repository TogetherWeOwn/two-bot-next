//! LFG reserved leave-action role-key refusal acceptance (TOG-12869).
//!
//! Pins parity section 13 gap (legacy 8b5d1e1, #414): LFG role keys must not
//! collide with the reserved leave action (`LFG_LEAVE_VALUE = "__leave__"`).
//! All assertions go through the public `two_bot_core::lfg` API only.
//! Pure offline: no DB, REST, router, timers, or feature flags. Runtime wiring
//! (commands/selects, defer-before-I/O) stays on TOG-10260.

use two_bot_core::lfg::{self, parse_role_spec, valid_role_key, RoleSpecError, LFG_LEAVE_VALUE};

#[test]
fn reserved_leave_value_is_the_legacy_sentinel() {
    assert_eq!(LFG_LEAVE_VALUE, "__leave__");
}

#[test]
fn parse_role_spec_refuses_exact_reserved_key() {
    assert_eq!(
        parse_role_spec("__leave__:Leave:1"),
        Err(RoleSpecError::ReservedKey("__leave__".to_owned()))
    );
    // Reserved key refuses wherever it appears in the spec.
    assert_eq!(
        parse_role_spec("tank:Tank:1,__leave__:Leave:1"),
        Err(RoleSpecError::ReservedKey("__leave__".to_owned()))
    );
}

#[test]
fn reserved_refusal_message_names_the_reservation() {
    let err = parse_role_spec("__leave__:Leave:1").expect_err("must refuse");
    let message = err.to_string();
    // The key renders inert (inspect-escaped): the sentinel's underscores
    // carry backslashes, so pin the escaped bytes, not the raw sentinel.
    assert!(
        message.contains("\\_\\_leave\\_\\_"),
        "message must name the key: {message}"
    );
    assert!(
        message.contains("reserved for leaving the group"),
        "message must name the reservation: {message}"
    );
}

#[test]
fn reserved_key_pattern_vs_reservation_layer() {
    // `valid_role_key` enforces only the `^[a-z0-9_-]{1,32}$` shape, so the
    // sentinel itself is shape-valid; the reservation is enforced by
    // `parse_role_spec` (ReservedKey), keeping the leave action distinct.
    assert!(valid_role_key(LFG_LEAVE_VALUE));
    assert_eq!(
        parse_role_spec("__leave__:Leave:1"),
        Err(RoleSpecError::ReservedKey(LFG_LEAVE_VALUE.to_owned()))
    );
}

#[test]
fn representative_valid_keys_accept() {
    for key in ["tank", "healer", "dps", "leave", "tank-2", "dps_1"] {
        assert!(valid_role_key(key), "{key} must be shape-valid");
        let spec = format!("{key}:Label:1");
        let roles = parse_role_spec(&spec).expect("valid key must parse");
        assert_eq!(roles.len(), 1);
        assert_eq!(roles[0].key, key);
    }
}

// Preview must share these validators and this renderer with publication.
// Literal outcomes keep a second implementation from silently drifting.
#[test]
fn preview_core_contract_normalizes_input_and_renders_the_creation_body() {
    let title = lfg::validate_title("  Friday raid  ").unwrap();
    assert_eq!(title, "Friday raid");
    let start = lfg::normalize_starts_at("2099-09-11T22:00:00+02:00", 0).unwrap();
    assert_eq!(start, "2099-09-11T20:00:00.000Z");
    let specs = parse_role_spec(" TANK : Tank : 2 , healer : Healer : 1 ").unwrap();
    assert_eq!(
        specs,
        vec![
            lfg::LfgRoleSpec {
                key: "tank".into(),
                label: "Tank".into(),
                slots: 2
            },
            lfg::LfgRoleSpec {
                key: "healer".into(),
                label: "Healer".into(),
                slots: 1
            },
        ]
    );
    let post = lfg::LfgPost {
        id: "fixture-only".into(),
        guild_id: "2222".into(),
        channel_id: "4444".into(),
        message_id: None,
        title,
        starts_at: start,
        status: lfg::LfgStatus::Open,
        created_by: "3333".into(),
        created_at: "2026-10-01T00:00:00Z".into(),
        closed_at: None,
    };
    let roles = lfg::spec_roles(&post.id, &specs);
    assert_eq!(
        lfg::lfg_content(&post, &roles, &[]),
        "**Friday raid** — Open\nStarts <t:4092840000:F>\n**Tank** 0/2\n**Healer** 0/1\nLFG id: `fixture-only` — close with `/lfg-close`"
    );
}

#[test]
fn preview_core_contract_bounds_utf16_and_refuses_invalid_future_times_and_roles() {
    assert!(lfg::validate_title(&"😀".repeat(50)).is_ok());
    assert_eq!(
        lfg::validate_title(&"😀".repeat(51)),
        Err(lfg::TitleError::BadTitle)
    );
    assert_eq!(lfg::validate_title("   "), Err(lfg::TitleError::BadTitle));
    let now = 4_092_840_000_000;
    assert_eq!(
        lfg::normalize_starts_at("2099-09-11T20:00:00Z", now),
        Err(lfg::StartsAtError::NotFuture)
    );
    assert_eq!(
        lfg::normalize_starts_at("2000-01-01T00:00:00Z", now),
        Err(lfg::StartsAtError::NotFuture)
    );
    assert_eq!(
        lfg::normalize_starts_at("invalid @everyone", now),
        Err(lfg::StartsAtError::NotIso8601)
    );
    assert_eq!(
        lfg::normalize_starts_at("2099-09-11T20:00:00.001Z", now).unwrap(),
        "2099-09-11T20:00:00.001Z"
    );
    for (input, error) in [
        (
            "TANK:Tank:2, tank :Other:1".into(),
            RoleSpecError::DuplicateKey("tank".into()),
        ),
        ("tank:Tank:0".into(), RoleSpecError::BadSlots),
        ("tank:Tank:100".into(), RoleSpecError::BadSlots),
        ("tank::1".into(), RoleSpecError::BadLabel),
        (
            format!("tank:{}:1", "😀".repeat(41)),
            RoleSpecError::BadLabel,
        ),
        (
            "__leave__:Leave:1".into(),
            RoleSpecError::ReservedKey("__leave__".into()),
        ),
        ("x".repeat(2340), RoleSpecError::TooLong),
    ] {
        assert_eq!(parse_role_spec(&input), Err(error), "input: {input}");
    }
    let largest = (0..20)
        .map(|n| format!("{n:032}:{}:99", "😀".repeat(40)))
        .collect::<Vec<_>>()
        .join(",");
    assert_eq!(largest.encode_utf16().count(), 2339);
    assert_eq!(parse_role_spec(&largest).unwrap().len(), 20);
    assert_eq!(
        parse_role_spec(&(largest + " ")),
        Err(RoleSpecError::TooLong)
    );
}

#[test]
fn preview_registration_adds_only_an_optional_boolean_without_new_help_commands() {
    use two_bot_core::{InteractionRouter, RouterGates};
    let router = InteractionRouter::new(RouterGates {
        configured_guild: Some(2222),
        announcements: true,
        scorecard: false,
        automations: false,
        moderation: false,
        voice: false,
        voice_assistant: false,
        tickets: false,
        self_roles: false,
        onboarding_picker: false,
        session_picker: false,
    });
    let definitions = router.publish_set(&[]).unwrap();
    let command = definitions.iter().find(|c| c.name == "lfg").unwrap();
    assert_eq!(
        command.default_member_permissions.as_deref(),
        Some("8589934592")
    );
    assert!(!command.dm_permission);
    assert_eq!(
        command.options.len(),
        4,
        "publish preview, not an undocumented wire-only switch"
    );
    let option = &command.options[3];
    assert_eq!(option.name, "preview");
    assert_eq!(option.kind, 5, "Discord boolean, not a truthy string");
    assert!(!option.required.unwrap_or(false));
    for (option, name) in command.options[..3]
        .iter()
        .zip(["title", "starts-at", "roles"])
    {
        assert_eq!(option.name, name);
        assert_eq!(option.kind, 3);
        assert!(option.required.unwrap_or(false));
    }
    let help = two_bot_core::help::help_text(&definitions);
    let tokens: Vec<_> = help
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '/' || c == '-' || c == '_'))
        .filter(|token| token.starts_with('/'))
        .collect();
    assert_eq!(tokens.iter().filter(|token| **token == "/lfg").count(), 1);
    assert!(!tokens.contains(&"/lfg-preview") && !tokens.contains(&"/lfg-publish"));
    assert!(!definitions
        .iter()
        .any(|c| c.name == "lfg-preview" || c.name == "lfg-publish"));
}
