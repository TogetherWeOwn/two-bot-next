//! Protected publication/help acceptance for increase-only LFG seat editing.
//! No global command census and no implementation compatibility shim.
use serde_json::json;

#[test]
fn command_advertises_optional_stable_role_and_bounded_absolute_seats_without_new_permissions() {
    let defs = two_bot_core::announcement_commands();
    let def = defs
        .iter()
        .find(|d| d.name == "lfg-edit")
        .expect("existing edit command");
    assert!(!def.dm_permission);
    assert_eq!(
        def.default_member_permissions.as_deref(),
        Some("8589934592")
    );
    let role = def
        .options
        .iter()
        .find(|o| o.name == "role")
        .expect("missing stable role selector");
    let seats = def
        .options
        .iter()
        .find(|o| o.name == "seats")
        .expect("missing absolute seat target");
    assert_eq!(
        role.kind, 3,
        "role is the existing signup key, not a Discord role snowflake"
    );
    assert_ne!(
        role.required,
        Some(true),
        "metadata-only editing remains available"
    );
    assert!(role.max_length.is_some_and(|n| n > 0 && n <= 128));
    assert_eq!(seats.kind, 4, "absolute seats use a strict integer option");
    assert_ne!(seats.required, Some(true));
    let wire = serde_json::to_value(seats).expect("published seats option");
    assert_eq!(wire["min_value"], json!(1));
    assert_eq!(wire["max_value"], json!(99));
    for name in ["id", "title", "starts-at"] {
        assert!(
            def.options.iter().any(|o| o.name == name),
            "lost existing metadata option {name}"
        );
    }
}

#[test]
fn rendered_help_explains_the_role_selector_and_absolute_target_in_the_edit_usage() {
    let defs = two_bot_core::announcement_commands();
    let edit = defs
        .iter()
        .find(|d| d.name == "lfg-edit")
        .expect("edit definition");
    let help = edit.help_detail_text();
    let lower = help.to_lowercase();
    assert!(lower.contains("/lfg-edit"));
    for option in ["role", "seats"] {
        let line = lower
            .lines()
            .find(|line| {
                line.contains(option) && (line.contains("optional") || line.contains("required"))
            })
            .unwrap_or_else(|| panic!("rendered edit help has no typed {option} argument: {help}"));
        assert!(
            line.contains("optional"),
            "seat options cannot make metadata editing require a role: {help}"
        );
    }
    assert!(
        lower.contains("99"),
        "member-visible help must expose the capacity ceiling: {help}"
    );
    assert!(
        lower.contains("absolute") || lower.contains("total") || lower.contains("target"),
        "make clear that seats is not an increment: {help}"
    );
    assert!(help.encode_utf16().count() <= 6000);
}
