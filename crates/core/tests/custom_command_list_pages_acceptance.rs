//! Protected behavioral contract for the default custom-command list page.
//! Later pages are exercised through the real gateway in the bot acceptance.
//! Keep the existing formatter callable: legacy callers get page one, not a
//! raw truncated string. No quota or command-publication redesign is required.

use two_bot_core::custom_commands::{format_command_list, StoredCommand};
use two_bot_core::feature_commands::automation_commands;

fn row(name: String, trigger: Option<String>, enabled: bool) -> StoredCommand {
    StoredCommand {
        guild_id: "2222".to_owned(),
        name,
        description: "List fixture".to_owned(),
        template: "Hello".to_owned(),
        text_trigger: trigger,
        enabled,
    }
}

fn assert_whole_first_page(rows: &[StoredCommand], expected: &[String]) {
    let content = format_command_list(rows);
    assert!(content.encode_utf16().count() <= 2000, "{content}");
    let actual: Vec<_> = content
        .lines()
        .filter(|line| line.starts_with('/'))
        .collect();
    assert!(!actual.is_empty(), "page one must contain command rows");
    assert!(
        actual.len() < rows.len(),
        "the fixture requires later pages"
    );
    assert_eq!(
        actual,
        expected[..actual.len()],
        "page one is a contiguous prefix of complete rows, including state"
    );
    assert!(
        content.contains("Page 1 of "),
        "missing page/total: {content}"
    );
    assert!(
        content.contains("/command-list page:2"),
        "missing actionable next-page navigation: {content}"
    );
}

#[test]
fn custom_list_acceptance_publishes_optional_one_based_integer_page() {
    let definition = automation_commands()
        .into_iter()
        .find(|definition| definition.name == "command-list")
        .expect("compiled command-list definition");
    let wire = serde_json::to_value(definition).expect("public command wire shape");
    assert_eq!(wire["default_member_permissions"], "32");
    let options = wire["options"]
        .as_array()
        .expect("page option is published");
    let page = options
        .iter()
        .find(|option| option["name"] == "page")
        .expect("administrators can actually select a page");
    assert_eq!(page["type"], 4, "Discord Integer, not a free-text option");
    assert_ne!(page["required"], true, "omission remains page one");
    assert_eq!(page["min_value"], 1);
}

#[test]
fn custom_list_acceptance_maximum_legal_rows_never_end_mid_name_trigger_or_state() {
    let rows: Vec<_> = (0..49)
        .map(|index| {
            let name = format!("c{index:02}{}", "x".repeat(29));
            row(name.clone(), Some(format!("!{name}")), index % 2 == 0)
        })
        .collect();
    // Independent row oracle: 32 name + 33 trigger + literal framing/state.
    let expected: Vec<_> = (0..49)
        .map(|index| {
            let suffix = "x".repeat(29);
            let state = if index % 2 == 0 { "on" } else { "off" };
            format!("/c{index:02}{suffix} (or !c{index:02}{suffix}) — {state}")
        })
        .collect();
    assert_whole_first_page(&rows, &expected);
}

#[test]
fn custom_list_acceptance_utf16_budget_includes_astral_width_and_navigation() {
    // Defensive formatter input, not a claim that emoji command names pass
    // validation. Legal command names/triggers are ASCII; the pure renderer
    // must nevertheless count UTF-16 rather than scalar values or UTF-8 bytes.
    let rows: Vec<_> = (0..49)
        .map(|index| row(format!("c{index:02}{}", "😀".repeat(29)), None, true))
        .collect();
    let expected: Vec<_> = (0..49)
        .map(|index| format!("/c{index:02}{} — on", "😀".repeat(29)))
        .collect();
    assert_whole_first_page(&rows, &expected);
}

#[test]
fn custom_list_acceptance_small_mixed_set_keeps_disabled_and_slash_only_rows() {
    let rows = vec![
        row("alpha".to_owned(), None, false),
        row("beta".to_owned(), Some("!b".to_owned()), true),
    ];
    let content = format_command_list(&rows);
    let lines: Vec<_> = content
        .lines()
        .filter(|line| line.starts_with('/'))
        .collect();
    assert_eq!(lines, ["/alpha — off", "/beta (or !b) — on"]);
    assert!(content.contains("Page 1 of 1"), "{content}");
    assert!(
        !content.contains("/command-list page:2"),
        "no phantom next page"
    );
    assert!(content.encode_utf16().count() <= 2000);
}

#[test]
fn custom_list_acceptance_empty_reply_stays_backward_compatible() {
    assert_eq!(format_command_list(&[]), "No custom commands defined.");
}
