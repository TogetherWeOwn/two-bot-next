//! Protected publication contract for the private saved-expiry inventory.
//! Catches an absent/unpublished command, broad authority, or unbounded selectors.

use two_bot_core::commands::{CommandOptionType, PERM_BAN_MEMBERS};
use two_bot_core::moderation::moderation_commands;

#[test]
fn tempban_inventory_is_a_private_staff_moderation_command_with_bounded_pages() {
    let definitions = moderation_commands();
    let definition = definitions
        .iter()
        .find(|definition| definition.name == "tempban-list")
        .expect("saved temporary-ban expiry inventory must be discoverable as /tempban-list");
    assert!(!definition.dm_permission, "guild-only inventory");
    assert_eq!(
        definition.default_member_permissions,
        Some(PERM_BAN_MEMBERS.to_string()),
        "Ban Members, not self/member or generic server-management authority"
    );
    assert_eq!(definition.options.len(), 1, "page only; no action selector");
    let page = &definition.options[0];
    assert_eq!(page.name, "page");
    assert_eq!(page.kind, CommandOptionType::Integer.as_u8());
    assert_ne!(page.required, Some(true), "omitted page selects page one");
    assert_eq!(page.min_value, Some(1));
    assert_eq!(page.max_value, Some(10_000));
    assert!(page.options.is_empty(), "no nested mutation surface");
    let help = definition.help_detail_text().to_lowercase();
    assert!(help.contains("page"), "help explains published paging");
    assert!(
        help.contains("saved") && (help.contains("expiry") || help.contains("expir")),
        "discovery describes saved expiry work, not observed unbans"
    );
}

#[test]
fn existing_tempban_mutation_definition_remains_a_positive_control() {
    let definitions = moderation_commands();
    let definition = definitions
        .iter()
        .find(|definition| definition.name == "tempban")
        .expect("existing tempban mutation is retained");
    assert_eq!(
        definition.default_member_permissions,
        Some(PERM_BAN_MEMBERS.to_string())
    );
    assert!(!definition.dm_permission);
    for name in ["target", "duration_seconds", "reason"] {
        assert!(
            definition
                .options
                .iter()
                .any(|option| option.name == name && option.required == Some(true)),
            "existing required input {name} remains"
        );
    }
}
