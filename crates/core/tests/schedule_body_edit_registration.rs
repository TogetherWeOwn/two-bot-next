//! Protected acceptance for inspecting and correcting an existing schedule.
//! The runtime battery exercises these published options against the real DB.

use two_bot_core::automation_commands;

#[test]
fn schedule_body_edit_registration_is_bounded_body_only_and_management_only() {
    let definitions = automation_commands();
    for (name, expected) in [
        (
            "schedule-inspect",
            vec![("id", 128, true), ("view", 8, false)],
        ),
        (
            "schedule-edit",
            vec![
                ("id", 128, true),
                ("revision", 128, true),
                ("body", 2000, true),
            ],
        ),
    ] {
        let definition = definitions
            .iter()
            .find(|d| d.name == name)
            .unwrap_or_else(|| panic!("missing management command /{name}"));
        assert_eq!(definition.default_member_permissions.as_deref(), Some("32"));
        assert!(
            !definition.dm_permission,
            "saved definitions stay guild-private"
        );
        let actual: Vec<_> = definition
            .options
            .iter()
            .map(|o| {
                assert_eq!(o.kind, 3, "{name}/{} is a bounded string", o.name);
                (
                    o.name.as_str(),
                    o.max_length.expect("published string cap"),
                    o.required == Some(true),
                )
            })
            .collect();
        assert_eq!(
            actual, expected,
            "edit must not expose timing, destination or recurrence changes"
        );
    }
}
