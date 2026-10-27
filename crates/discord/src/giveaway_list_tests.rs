//! Renderer boundary tests supplement the protected dispatched-list acceptance.
#![cfg(test)]

use super::{
    list::{list_prize, render_active_list},
    *,
};
use two_bot_core::message_safety;

fn saved_row(index: usize) -> GiveawayRow {
    let at = OffsetDateTime::from_unix_timestamp(253_402_300_799).expect("fixture time");
    GiveawayRow {
        guild_id: u64::MAX.to_string(),
        id: format!("draw-{index:02}-{}", "x".repeat(56)),
        prize: "@everyone 🎁 *prize* ".repeat(10),
        channel_id: u64::MAX.to_string(),
        message_id: u64::MAX.to_string(),
        created_by: "1".into(),
        winners_count: 1,
        ends_at: at,
        status: "open".into(),
        requirements_json: serde_json::json!({}),
        max_entries: 100,
        draw_nonce: "not-for-display".into(),
        claim_token: Some("not-for-display".into()),
        claimed_at: None,
        paused_at: None,
        announced_at: None,
        created_at: at,
        updated_at: at,
    }
}

#[test]
fn ten_maximum_ids_and_links_survive_final_outbound_normalization() {
    let rows: Vec<_> = (0..10).map(saved_row).collect();
    let text = render_active_list(&rows);
    assert_ne!(text, FAILURE);
    assert!(message_safety::text_len(&text) <= message_safety::CONTENT_LIMIT);
    assert_eq!(message_safety::content(&text), text, "no final truncation");
    assert_eq!(text.lines().count(), 11);
    for row in rows {
        let line = text
            .lines()
            .find(|line| line.contains(&row.id))
            .expect("complete ID");
        assert!(line.contains(&format!(
            "https://discord.com/channels/{}/{}/{}",
            row.guild_id, row.channel_id, row.message_id
        )));
        assert!(line.contains("open <t:253402300799:f>"));
        assert!(line.contains('…'));
    }
    assert!(!text.contains("not-for-display"));
}

#[test]
fn ten_normal_rows_preserve_prize_prefix_and_no_countdown_for_paused() {
    let mut rows: Vec<_> = (0..10).map(saved_row).collect();
    for (index, row) in rows.iter_mut().enumerate() {
        row.guild_id = "1234".into();
        row.prize = format!("Prize{index:02} @everyone `\n{}", "🎁".repeat(178));
        if index % 2 == 0 {
            row.status = "paused".into();
        } else {
            row.message_id = "pending".into();
        }
    }
    let text = render_active_list(&rows);
    assert_eq!(message_safety::content(&text), text);
    for (index, row) in rows.iter().enumerate() {
        let line = text
            .lines()
            .find(|line| line.contains(&row.id))
            .expect("complete ID");
        assert!(line.contains(&format!("Prize{index:02}")));
        assert!(line.contains('…'));
        if index % 2 == 0 {
            assert!(line.contains("paused"));
            assert!(!line.contains("<t:"));
        } else {
            assert!(line.contains("message unconfirmed"));
            assert!(!line.contains("https://discord.com"));
        }
    }
}

#[test]
fn malformed_saved_snowflakes_never_become_links() {
    for invalid in [
        "",
        "pending",
        "0",
        "01",
        "+1",
        "-1",
        "18446744073709551616",
        "1/2",
        "1\n2",
    ] {
        let mut row = saved_row(0);
        row.message_id = invalid.into();
        let text = render_active_list(&[row.clone()]);
        assert!(text.contains("message unconfirmed"), "{invalid:?}");
        assert!(!text.contains("https://discord.com"));
        row.channel_id = invalid.into();
        let text = render_active_list(&[row.clone()]);
        assert!(text.contains("destination unavailable"), "{invalid:?}");
        assert!(!text.contains("https://discord.com"));
        row.channel_id = "1".into();
        row.message_id = "2".into();
        row.guild_id = invalid.into();
        let text = render_active_list(&[row]);
        assert!(text.contains("destination unavailable"), "{invalid:?}");
        assert!(!text.contains("https://discord.com"));
    }
}

#[test]
fn prize_elision_counts_escaping_astral_units_and_mention_growth() {
    assert_eq!(list_prize("Small prize", 32), "Small prize");
    assert_eq!(list_prize("🎁🎁🎁", 5), "🎁🎁…");
    assert_eq!(list_prize("`*rest", 5), "\\`\\*…");
    assert_eq!(list_prize("@everyone rest", 10), "@\u{200b}everyon…");
    assert_eq!(list_prize("reward", 1), "…");
    assert_eq!(
        list_prize("x".repeat(33).as_str(), 100),
        format!("{}…", "x".repeat(32))
    );
    assert_eq!(list_prize("x".repeat(32).as_str(), 100), "x".repeat(32));
    assert_eq!(list_prize("\n\r\u{2028}\u{2029}", 10), "?");
    assert_eq!(list_prize("a\n\r\u{2028}\u{2029}b", 10), "ab");
    // Invisible-only and control-plus-space prizes have no displayable text.
    assert_eq!(list_prize("\u{200b}\u{2060}", 10), "?");
    assert_eq!(list_prize("\u{200b} \n\u{feff}\t", 10), "?");
    // Mention neutralization may shrink the preview to exactly the bound; the
    // cut remainder must still carry the explicit ellipsis.
    let split_mention: String = "@"
        .chars()
        .chain("everyone".chars().flat_map(|c| ['\u{200b}', c]))
        .collect();
    assert_eq!(split_mention.chars().count(), 17);
    assert_eq!(
        list_prize(&format!("{split_mention}abcdefghijklmnopTAIL"), 100),
        "@\u{200b}everyoneabcdefghijklmnop…"
    );
    for prize in [
        "🎁".repeat(200),
        "`\\*".repeat(70),
        "@everyone @here ".repeat(15),
    ] {
        for budget in 1..=100 {
            let text = list_prize(&prize, budget);
            assert!(message_safety::text_len(&text) <= budget);
            assert_eq!(message_safety::neutralize_mentions(&text), text);
            assert!(!text.contains('\n'));
        }
    }
}

#[test]
fn empty_list_and_damaged_unrenderable_identity_are_not_partial_successes() {
    assert_eq!(render_active_list(&[]), "No open giveaways.");
    for invalid in ["", "id\nforged", "id`forged", &"x".repeat(65)] {
        let mut row = saved_row(0);
        row.id = invalid.into();
        assert_eq!(render_active_list(&[row]), FAILURE);
    }
    assert_eq!(
        render_active_list(&(0..11).map(saved_row).collect::<Vec<_>>()),
        FAILURE
    );
    // Short rows fit the budget, so only the row cap can refuse these.
    let short_rows: Vec<_> = (0..11)
        .map(|index| {
            let mut row = saved_row(index);
            row.id = format!("d{index}");
            row.prize = "x".into();
            row.guild_id = "1".into();
            row.channel_id = "2".into();
            row.message_id = "3".into();
            row
        })
        .collect();
    assert_eq!(render_active_list(&short_rows), FAILURE);
    assert_ne!(render_active_list(&short_rows[..10]), FAILURE);
    let mut row = saved_row(0);
    row.status = "damaged".into();
    assert_eq!(render_active_list(&[row]), FAILURE);
}
