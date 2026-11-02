//! Protected discovery contract for the feed pause/resume lifecycle.
//! Missing lifecycle commands and hidden paused state must fail behaviorally.

use two_bot_core::{
    announcement_commands,
    feeds::{feed_list_page, FeedKind, FeedRelay},
    help::help_text,
};

fn relay(index: usize, enabled: bool) -> FeedRelay {
    FeedRelay {
        id: format!("relay-{index:032x}"),
        guild_id: "2222".into(),
        channel_id: "3333".into(),
        kind: FeedKind::Rss,
        source: format!("https://example.org/news/section-{index:02}/updates?format=rss"),
        enabled,
        last_checked_at: Some(1234),
        created_by: "4444".into(),
        created_at: index as i64,
        updated_at: 1234,
    }
}

#[test]
fn lifecycle_registration_requires_the_same_guild_management_authority_and_stable_id() {
    let definitions = announcement_commands();
    let remove = definitions
        .iter()
        .find(|d| d.name == "feed-remove")
        .unwrap();
    for name in ["feed-pause", "feed-resume"] {
        let command = definitions
            .iter()
            .find(|d| d.name == name)
            .unwrap_or_else(|| panic!("missing guild-admin lifecycle command {name}"));
        assert_eq!(command.default_member_permissions.as_deref(), Some("32"));
        assert_eq!(
            command.default_member_permissions,
            remove.default_member_permissions
        );
        assert!(!command.dm_permission, "lifecycle mutation is guild-only");
        assert_eq!(
            command.options.len(),
            1,
            "reuse a stable relay ID, not a URL or operator flag"
        );
        let id = &command.options[0];
        assert_eq!(id.name, "id");
        assert_eq!(id.kind, 3, "stable IDs are strings");
        assert_eq!(id.required, Some(true));
        assert!(!command.description.trim().is_empty());
    }
}

#[test]
fn live_help_groups_pause_and_resume_with_manage_server_feed_commands() {
    let definitions = announcement_commands();
    let text = help_text(&definitions);
    let group = text
        .split("**Feed relays**")
        .nth(1)
        .expect("feed help group")
        .split("\n\n")
        .next()
        .unwrap();
    assert!(group.contains("Manage Server"));
    for name in [
        "/feed-add",
        "/feed-remove",
        "/feed-list",
        "/feed-pause",
        "/feed-resume",
    ] {
        assert!(
            group.contains(name),
            "live feed help omitted {name}: {group}"
        );
    }
}

#[test]
fn list_labels_both_states_on_their_own_stable_id_rows() {
    let rows = vec![relay(1, true), relay(2, false)];
    let text = feed_list_page(&rows, 1);
    for (row, state) in rows.iter().zip(["enabled", "paused"]) {
        let line = text
            .lines()
            .find(|line| line.starts_with(&format!("`{}`", row.id)))
            .expect("complete stable ID row");
        assert!(
            line.to_ascii_lowercase().contains(state),
            "missing {state} state: {line}"
        );
        assert!(line.contains(&row.source));
        assert!(line.contains("rss"));
        assert!(line.contains("<#3333>"));
    }
}

#[test]
fn lifecycle_labels_preserve_bounded_whole_row_discovery_for_restored_excess() {
    let rows: Vec<_> = (1..=35).map(|index| relay(index, index % 2 == 0)).collect();
    let mut displayed = Vec::new();
    for number in 1..=35 {
        let page = feed_list_page(&rows, number);
        assert!(
            page.encode_utf16().count() <= 2000,
            "page {number} exceeds Discord's bound"
        );
        let lines: Vec<_> = page.lines().filter(|line| line.starts_with('`')).collect();
        if lines.is_empty() {
            break;
        }
        for line in lines {
            let id = line.strip_prefix('`').unwrap().split('`').next().unwrap();
            let row = rows
                .iter()
                .find(|row| row.id == id)
                .expect("whole stable ID, not truncation");
            let state = if row.enabled { "enabled" } else { "paused" };
            assert!(
                line.to_ascii_lowercase().contains(state),
                "missing {state}: {line}"
            );
            assert!(line.contains("rss") && line.contains("<#3333>"));
            assert!(
                line.contains(&format!("section-{:02}", row.created_at)),
                "source identity lost: {line}"
            );
            displayed.push(id.to_owned());
        }
    }
    assert_eq!(
        displayed,
        rows.iter().map(|row| row.id.clone()).collect::<Vec<_>>(),
        "every enabled/paused ID must remain discoverable exactly once in order"
    );
}
