//! Published-shape acceptance for `/schedule-list` pagination.
//!
//! A guild may hold 25 schedules, whose whole rows cannot fit in one
//! 2,000 UTF-16 unit Discord message. The locked contract (behavior is
//! pinned end-to-end in the bot crate's command-runtime tests) preserves
//! pagination and adds a channel filter — no new command name, no permission
//! change — so staff can walk bounded pages and discover schedule ids:
//!
//! - option `page`, integer, optional (omitted means page 1), minimum 1;
//! - option `here`, boolean, optional (omitted/false means the whole server);
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
fn schedule_list_publishes_optional_page_and_here_without_relaxing_permissions() {
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
        2,
        "keep page and add only here; got {:?}",
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
    let here = &schedule_list.options[1];
    assert_eq!(here.name, "here");
    assert_eq!(
        here.kind, 5,
        "here is a Discord boolean, not a caller-supplied channel"
    );
    assert_ne!(here.required, Some(true), "old invocations remain valid");
    assert!(here.min_value.is_none() && here.max_value.is_none());
    assert!(here.max_length.is_none() && here.choices.is_empty() && here.options.is_empty());
    assert!(here
        .description
        .to_ascii_lowercase()
        .contains("this channel"));
}

#[test]
fn schedule_here_help_explains_scope_and_default_only_while_published() {
    let mut definitions = automation_commands();
    let schedule_list = definitions
        .iter()
        .find(|definition| definition.name == "schedule-list")
        .expect("schedule-list publishes");
    // Option-level discovery lives in the confirmed-publication detail, not
    // the name-only list: the detail renders usage, per-argument types and
    // the declared permission from the published definition.
    let active = schedule_list.help_detail_text();
    assert!(
        active.contains("Usage: /schedule-list [page] [here]"),
        "formable usage names the optional filter: {active}"
    );
    // `inert` strips markdown punctuation, so the detail drops the
    // description's parentheses; scope and default survive verbatim.
    assert!(
        active
            .contains("- here (boolean, optional): Only schedules in this channel default: false"),
        "discover the boolean filter, its scope and its default: {active}"
    );
    assert!(
        active.contains("- page (integer, optional, min 1): Page of the schedule list 1-based"),
        "preserve the page argument: {active}"
    );
    assert!(
        active.contains("Permissions: Manage Server."),
        "retain the authorization requirement: {active}"
    );
    // Name-only list controls: exact-once discovery and the group hint stay
    // stable while options change underneath.
    let listed = two_bot_core::help_text(&definitions);
    assert_eq!(
        listed.matches("/schedule-list").count(),
        1,
        "retain exact-once discovery"
    );
    assert!(
        listed.contains("Manage Server"),
        "retain authorization hint"
    );
    // Withheld feature, withheld hint: without the published option the
    // detail never names the filter.
    let mut bare = schedule_list.clone();
    bare.options.pop();
    let withheld = bare.help_detail_text();
    assert!(
        !withheld.to_ascii_lowercase().contains("here"),
        "unpublished feature has no stale help hint: {withheld}"
    );
    definitions.retain(|definition| definition.name != "schedule-list");
    let unlisted = two_bot_core::help_text(&definitions);
    assert!(
        !unlisted.contains("/schedule-list"),
        "unpublished command leaves the list"
    );
}
