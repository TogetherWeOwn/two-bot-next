//! Published-shape acceptance for `/schedule-list` pagination.
//!
//! A guild may hold 25 schedules, whose whole rows cannot fit in one
//! 2,000 UTF-16 unit Discord message. The locked contract (behavior is
//! pinned end-to-end in the bot crate's command-runtime tests) adds ONE
//! additive option to the existing command — no new command name, no
//! permission change — so staff can walk bounded pages and discover every
//! configured schedule id:
//!
//! - option `page`, integer, optional (omitted means page 1), minimum 1;
//! - it is the command's only option;
//! - `ManageGuild` default permissions and guild-only shape are unchanged.
//!
//! Synthetic fixtures only: no Discord, network, or database.

use two_bot_core::automation_commands;

/// Discord's wire type for an integer option
/// (`CommandOptionType::Integer`).
const INTEGER_OPTION: u8 = 4;

/// `ManageGuild` permission bit, as published in
/// `default_member_permissions`.
const MANAGE_GUILD: &str = "32";

#[test]
fn schedule_list_publishes_one_optional_page_option() {
    let definitions = automation_commands();
    let schedule_list = definitions
        .iter()
        .find(|definition| definition.name == "schedule-list")
        .expect("schedule-list stays published under its existing name");

    assert_eq!(
        schedule_list.default_member_permissions.as_deref(),
        Some(MANAGE_GUILD),
        "pagination must not relax the ManageGuild gate"
    );
    assert!(
        !schedule_list.dm_permission,
        "schedule-list stays guild-only"
    );

    assert_eq!(
        schedule_list.options.len(),
        1,
        "page is the only additive option; got {:?}",
        schedule_list
            .options
            .iter()
            .map(|option| option.name.as_str())
            .collect::<Vec<_>>()
    );
    let page = &schedule_list.options[0];
    assert_eq!(page.name, "page");
    assert_eq!(page.kind, INTEGER_OPTION, "page is an integer option");
    assert_ne!(
        page.required,
        Some(true),
        "page stays optional so existing invocations keep working"
    );
    assert_eq!(
        page.min_value,
        Some(1),
        "Discord itself refuses page numbers below 1"
    );
    assert!(
        page.max_value.is_none_or(|max| max >= 2),
        "an advertised maximum must still let staff reach page 2, got {:?}",
        page.max_value
    );
    assert!(
        !page.description.trim().is_empty(),
        "the published option carries a description"
    );
}
