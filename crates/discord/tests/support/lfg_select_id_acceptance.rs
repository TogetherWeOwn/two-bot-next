#![cfg(test)]

//! Protected acceptance: malformed select IDs never become executable actions.
//! The boundary is 1..=128 UTF-16 units, matching the close interaction contract.

use two_bot_core::lfg::{parse_lfg_select, LfgSelectAction};

fn refuses_both_actions(post_id: &str) {
    for value in ["tank", "__leave__"] {
        assert_eq!(
            parse_lfg_select(&format!("two:lfg:{post_id}"), value),
            None,
            "invalid post ID must not produce a signup or leave action"
        );
    }
}

#[test]
fn empty_select_id_is_not_an_action() {
    refuses_both_actions("");
}

#[test]
fn ascii_select_id_over_128_units_is_not_an_action() {
    refuses_both_actions(&"a".repeat(129));
}

#[test]
fn multibyte_select_id_over_128_units_is_not_an_action() {
    refuses_both_actions(&"é".repeat(129));
}

#[test]
fn surrogate_pairs_count_as_two_units_in_select_ids() {
    refuses_both_actions(&"😀".repeat(65));
}

#[test]
fn overlong_select_id_with_a_mention_tail_is_not_an_action() {
    refuses_both_actions(&format!("{}@everyone", "x".repeat(128)));
}

#[test]
fn valid_select_ids_preserve_signup_and_leave_identity() {
    for post_id in [
        "lfg-proof".to_owned(),
        "a".repeat(128),
        "é".repeat(100),
        "😀".repeat(64),
        " ".to_owned(),
    ] {
        let custom_id = format!("two:lfg:{post_id}");
        assert_eq!(
            parse_lfg_select(&custom_id, "tank"),
            Some(LfgSelectAction::Signup {
                post_id: post_id.clone(),
                role_key: "tank".into(),
            })
        );
        assert_eq!(
            parse_lfg_select(&custom_id, "__leave__"),
            Some(LfgSelectAction::Leave { post_id })
        );
    }
    assert_eq!(parse_lfg_select("two:other:lfg-proof", "tank"), None);
}
