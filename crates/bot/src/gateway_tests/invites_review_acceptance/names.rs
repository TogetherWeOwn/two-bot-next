#![cfg(test)]

use serde_json::{json, Value};

use super::support::*;

async fn both_surfaces(body: Value, expected: String) {
    let mut config = Discord::default();
    config.members.insert(ALICE, body);
    with_fixture(config, move |f| {
        Box::pin(async move {
            f.credit(MEMBER, ALICE).await;
            // Obtain both production results before asserting, so a command failure
            // cannot hide a different welcome-name bug.
            let command = f.command(ALICE).await;
            let welcome = f.welcome(MEMBER).await;
            let command_name = text(&command)
                .lines()
                .next()
                .expect("personal row")
                .strip_suffix(": 1 attributed invites (net)")
                .expect("personal name slot");
            let welcome_name = text(&welcome)
                .rsplit_once("\n\nInvited by ")
                .expect("attribution paragraph")
                .1
                .strip_suffix('.')
                .expect("attribution sentence");
            assert_eq!(
                (command_name, welcome_name),
                (expected.as_str(), expected.as_str()),
                "command and welcome use the same safe, identity-bound display name"
            );
            assert!(
                command_name.encode_utf16().count() <= 48,
                "display name stays within 48 UTF-16 units"
            );
            assert!(
                welcome_name.encode_utf16().count() <= 48,
                "welcome display name stays within 48 UTF-16 units"
            );
        })
    })
    .await;
}

#[tokio::test]
async fn matching_member_nickname_precedes_global_and_username_on_both_surfaces() {
    both_surfaces(
        member(ALICE, json!("Nick"), json!("Global"), json!("Username")),
        "Nick".into(),
    )
    .await;
}

#[tokio::test]
async fn global_name_precedes_username_when_nickname_is_absent_on_both_surfaces() {
    both_surfaces(
        member(ALICE, Value::Null, json!("Global"), json!("Username")),
        "Global".into(),
    )
    .await;
}

#[tokio::test]
async fn username_is_the_last_available_display_name_on_both_surfaces() {
    both_surfaces(
        member(ALICE, Value::Null, Value::Null, json!("Username")),
        "Username".into(),
    )
    .await;
}

#[tokio::test]
async fn blank_nickname_falls_through_to_first_nonblank_global_name_on_both_surfaces() {
    both_surfaces(
        member(ALICE, json!(" \t"), json!("Global"), json!("Username")),
        "Global".into(),
    )
    .await;
}

#[tokio::test]
async fn blank_global_name_falls_through_to_username_on_both_surfaces() {
    both_surfaces(
        member(ALICE, Value::Null, json!("\n\t"), json!("Username")),
        "Username".into(),
    )
    .await;
}

#[tokio::test]
async fn all_blank_display_names_fall_back_to_the_requested_member_on_both_surfaces() {
    both_surfaces(
        member(ALICE, json!(" "), json!("\n"), json!("\t")),
        "Member 9004".into(),
    )
    .await;
}

#[tokio::test]
async fn missing_display_names_fall_back_to_the_requested_member_on_both_surfaces() {
    both_surfaces(json!({"user": {"id": "9004"}}), "Member 9004".into()).await;
}

#[tokio::test]
async fn a_different_user_id_cannot_supply_a_display_name_on_either_surface() {
    both_surfaces(
        member(
            BRAM,
            json!("Impostor Nick"),
            json!("Impostor Global"),
            json!("Impostor User"),
        ),
        "Member 9004".into(),
    )
    .await;
}

#[tokio::test]
async fn missing_user_id_cannot_supply_a_display_name_on_either_surface() {
    both_surfaces(
        json!({"nick": "Unbound", "user": {"username": "Unbound"}}),
        "Member 9004".into(),
    )
    .await;
}

#[tokio::test]
async fn multibyte_bmp_names_are_exactly_bounded_on_both_surfaces() {
    both_surfaces(
        member(ALICE, json!("界".repeat(49)), Value::Null, Value::Null),
        "界".repeat(48),
    )
    .await;
}

#[tokio::test]
async fn astral_names_use_utf16_not_scalar_or_byte_bounds_on_both_surfaces() {
    both_surfaces(
        member(ALICE, json!("😀".repeat(25)), Value::Null, Value::Null),
        "😀".repeat(24),
    )
    .await;
}

#[tokio::test]
async fn a_surrogate_pair_is_not_split_to_fill_the_last_utf16_unit_on_either_surface() {
    both_surfaces(
        member(
            ALICE,
            json!("a".repeat(47) + "😀tail"),
            Value::Null,
            Value::Null,
        ),
        "a".repeat(47),
    )
    .await;
}

#[tokio::test]
async fn multiline_names_cannot_forge_a_board_or_welcome_line() {
    both_surfaces(
        member(ALICE, json!("First\nSecond"), Value::Null, Value::Null),
        "First Second".into(),
    )
    .await;
}

#[tokio::test]
async fn markdown_and_mention_like_names_are_rendered_as_safe_literal_text_on_both_surfaces() {
    both_surfaces(
        member(
            ALICE,
            json!("*_[x]_*\n@here <@9005>"),
            Value::Null,
            Value::Null,
        ),
        "\\*\\_\\[x\\]\\_\\* @\u{200b}here <\u{200b}@9005>".into(),
    )
    .await;
}
