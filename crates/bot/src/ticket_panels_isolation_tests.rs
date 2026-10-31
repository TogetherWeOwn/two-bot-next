//! Acceptance tests: one malformed channel-document message must be
//! isolated (skipped) instead of failing the whole `panels` batch, and
//! per-message `custom_ids` must be bounded. Valid messages in the same
//! batch still produce panels. `valid_id` / `member_overwrite` strictness
//! is pinned unchanged. No database, no network: pure `panels` inputs.
use super::*;
use serde_json::json;

/// Discord allows at most 5 action rows of 5 buttons on one message, so a
/// legitimate message never carries more than 25 button custom ids.
const MAX_CUSTOM_IDS_PER_MESSAGE: usize = 25;

fn message_with_buttons(author: &str, custom_ids: &[&str]) -> Value {
    let buttons: Vec<Value> = custom_ids
        .iter()
        .map(|id| json!({"type": 2, "custom_id": id}))
        .collect();
    json!({
        "author": {"id": author},
        "components": [{"type": 1, "components": buttons}],
    })
}

fn flood_message(author: &str, count: usize) -> Value {
    let buttons: Vec<Value> = (0..count)
        .map(|i| json!({"type": 2, "custom_id": format!("flood-{i}")}))
        .collect();
    json!({
        "author": {"id": author},
        "components": [{"type": 1, "components": buttons}],
    })
}

fn panel(author: &str, custom_ids: &[&str]) -> PanelMessage {
    PanelMessage {
        author_id: author.to_owned(),
        custom_ids: custom_ids.iter().map(|id| (*id).to_owned()).collect(),
    }
}

#[test]
fn all_valid_batch_preserves_order_authors_and_custom_ids() {
    let got = panels(vec![
        message_with_buttons("400", &[TICKET_OPEN_ID]),
        json!({"author": {"id": "500"}, "content": "ordinary message"}),
        message_with_buttons("600", &[TICKET_CLAIM_ID, TICKET_CLOSE_ID]),
    ])
    .ok()
    .expect("all-valid batch stays Ok");
    assert_eq!(
        got,
        vec![
            panel("400", &[TICKET_OPEN_ID]),
            panel("500", &[]),
            panel("600", &[TICKET_CLAIM_ID, TICKET_CLOSE_ID]),
        ]
    );
    // The bot's own live panel is recognised: no duplicate panel is posted.
    assert!(!panel_needed("400", &got));
}

#[test]
fn empty_batch_returns_empty_ok() {
    let got = panels(Vec::new()).ok().expect("empty batch stays Ok");
    assert!(got.is_empty());
    assert!(panel_needed("400", &got));
}

#[test]
fn empty_components_array_keeps_the_message_with_no_ids() {
    let got = panels(vec![json!({"author": {"id": "500"}, "components": []})])
        .ok()
        .expect("empty components stays Ok");
    assert_eq!(got, vec![panel("500", &[])]);
}

#[test]
fn one_malformed_middle_message_is_skipped_and_valid_rows_still_panel() {
    let got = panels(vec![
        message_with_buttons("600", &[TICKET_CLAIM_ID, TICKET_CLOSE_ID]),
        json!({"author": {"id": "500"}, "components": {}}),
        message_with_buttons("400", &[TICKET_OPEN_ID]),
    ])
    .ok()
    .expect("one malformed message no longer fails the batch");
    assert_eq!(
        got,
        vec![
            panel("600", &[TICKET_CLAIM_ID, TICKET_CLOSE_ID]),
            panel("400", &[TICKET_OPEN_ID]),
        ]
    );
    // Panel repair is not stalled and no duplicate panel is posted.
    assert!(!panel_needed("400", &got));
}

#[test]
fn malformed_first_and_last_messages_do_not_fail_the_batch() {
    let got = panels(vec![
        json!({"components": []}),
        message_with_buttons("400", &[TICKET_OPEN_ID]),
        json!({"author": {"id": 500}}),
    ])
    .ok()
    .expect("malformed edges no longer fail the batch");
    assert_eq!(got, vec![panel("400", &[TICKET_OPEN_ID])]);
}

#[test]
fn malformed_author_variants_are_each_skipped() {
    for malformed in [
        json!({"components": []}),
        json!({"author": {}, "components": []}),
        json!({"author": {"id": 500}, "components": []}),
        json!({"author": {"id": null}, "components": []}),
        json!({"author": "500", "components": []}),
    ] {
        let got = panels(vec![
            message_with_buttons("400", &[TICKET_OPEN_ID]),
            malformed.clone(),
        ])
        .ok()
        .unwrap_or_else(|| panic!("malformed author variant must be skipped: {malformed}"));
        assert_eq!(got, vec![panel("400", &[TICKET_OPEN_ID])], "{malformed}");
    }
}

#[test]
fn malformed_component_shape_variants_are_each_skipped() {
    for malformed in [
        json!({"author": {"id": "500"}, "components": {}}),
        json!({"author": {"id": "500"}, "components": "rows"}),
        json!({"author": {"id": "500"}, "components": null}),
        json!({"author": {"id": "500"}, "components": [{"components": {}}]}),
        json!({"author": {"id": "500"}, "components": [{"components": "buttons"}]}),
        json!({"author": {"id": "500"}, "components": [{"type": 1}]}),
    ] {
        let got = panels(vec![
            malformed.clone(),
            message_with_buttons("400", &[TICKET_OPEN_ID]),
        ])
        .ok()
        .unwrap_or_else(|| panic!("malformed components variant must be skipped: {malformed}"));
        assert_eq!(got, vec![panel("400", &[TICKET_OPEN_ID])], "{malformed}");
    }
}

#[test]
fn a_message_with_one_malformed_row_is_skipped_wholesale_not_partially_kept() {
    // The first row parses, the second does not: isolation is per message,
    // so the partially parsed message must not leak its first row's ids.
    let got = panels(vec![
        json!({
            "author": {"id": "400"},
            "components": [
                {"type": 1, "components": [{"type": 2, "custom_id": TICKET_OPEN_ID}]},
                {"type": 1, "components": {}},
            ],
        }),
        message_with_buttons("600", &[TICKET_CLAIM_ID]),
    ])
    .ok()
    .expect("partially malformed message is isolated, batch stays Ok");
    assert_eq!(got, vec![panel("600", &[TICKET_CLAIM_ID])]);
    // The dropped bot message does not count as a live panel: repair reposts.
    assert!(panel_needed("400", &got));
}

#[test]
fn all_malformed_batch_returns_empty_ok_not_invalid_evidence() {
    let got = panels(vec![
        json!({"author": {"id": "500"}, "components": {}}),
        json!({"components": []}),
        json!({"author": {"id": 500}}),
        json!("not-a-message-object"),
        json!(null),
        json!([1, 2, 3]),
    ])
    .ok()
    .expect("all-malformed batch returns Ok(empty), not an error");
    assert!(got.is_empty());
    // With no usable evidence of a live panel, repair may repost one.
    assert!(panel_needed("400", &got));
}

#[test]
fn buttons_without_a_string_custom_id_are_ignored_not_fatal() {
    // Link buttons carry a url instead of a custom_id, and foreign
    // components are common in real channels: they contribute no id and
    // must neither fail the batch nor discard the rest of the message.
    let got = panels(vec![json!({
        "author": {"id": "400"},
        "components": [{
            "type": 1,
            "components": [
                {"type": 2, "style": 5, "url": "https://example.test/"},
                {"type": 2, "custom_id": 42},
                {"type": 2, "custom_id": TICKET_OPEN_ID},
                "not-a-button-object",
            ],
        }],
    })])
    .ok()
    .expect("id-less buttons stay tolerated");
    assert_eq!(got, vec![panel("400", &[TICKET_OPEN_ID])]);
    assert!(!panel_needed("400", &got));
}

#[test]
fn exactly_at_the_custom_id_bound_every_id_is_kept() {
    let ids: Vec<String> = (0..MAX_CUSTOM_IDS_PER_MESSAGE)
        .map(|i| format!("id-{i}"))
        .collect();
    let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
    let got = panels(vec![message_with_buttons("500", &refs)])
        .ok()
        .expect("at-bound message stays Ok");
    assert_eq!(got, vec![panel("500", &refs)]);
}

#[test]
fn over_bound_custom_ids_truncate_to_the_bound_preserving_the_first_ids() {
    let mut ids: Vec<String> = vec![TICKET_OPEN_ID.to_owned()];
    ids.extend((0..29).map(|i| format!("extra-{i}")));
    let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
    let got = panels(vec![message_with_buttons("400", &refs)])
        .ok()
        .expect("over-bound message truncates, never fails the batch");
    assert_eq!(got.len(), 1, "the message itself is kept, not skipped");
    assert_eq!(got[0].custom_ids.len(), MAX_CUSTOM_IDS_PER_MESSAGE);
    assert_eq!(
        got[0].custom_ids,
        ids[..MAX_CUSTOM_IDS_PER_MESSAGE].to_vec(),
        "truncation keeps the earliest ids in order"
    );
    // A real panel id inside the bound still recognises the live panel.
    assert!(!panel_needed("400", &got));
}

#[test]
fn a_custom_id_flood_is_bounded_and_the_rest_of_the_batch_survives() {
    let got = panels(vec![
        flood_message("500", 5_000),
        message_with_buttons("400", &[TICKET_OPEN_ID]),
    ])
    .ok()
    .expect("flooded message never fails the batch");
    assert_eq!(got.len(), 2);
    assert!(
        got[0].custom_ids.len() <= MAX_CUSTOM_IDS_PER_MESSAGE,
        "custom_ids must be bounded, got {}",
        got[0].custom_ids.len()
    );
    assert_eq!(got[1], panel("400", &[TICKET_OPEN_ID]));
}

#[test]
fn valid_id_strictness_is_unchanged() {
    assert_eq!(valid_id("123").ok(), Some(123));
    assert_eq!(valid_id(&u64::MAX.to_string()).ok(), Some(u64::MAX));
    for bad in ["0", "0123", "abc", "", " 123", "123 ", "-1"] {
        assert!(valid_id(bad).is_err(), "{bad:?} must stay refused");
    }
    let overflow = format!("{}0", u64::MAX);
    assert!(valid_id(&overflow).is_err(), "overflow must stay refused");
}

#[test]
fn member_overwrite_strictness_is_unchanged() {
    let doc = json!({
        "permission_overwrites": [
            {"id": "500", "type": 1, "allow": "1024", "deny": "8"},
            {"id": "300", "type": 0, "allow": "0", "deny": "0"},
        ],
    });
    assert_eq!(member_overwrite(&doc, "500").ok(), Some((1024, 8)));
    // A role row (type 0) is not a member overwrite, and a stranger has none.
    assert!(member_overwrite(&doc, "300").is_err());
    assert!(member_overwrite(&doc, "999").is_err());
    for bad_doc in [
        json!({}),
        json!({"permission_overwrites": {}}),
        json!({"permission_overwrites": [{"id": "500", "type": 1, "allow": "x", "deny": "8"}]}),
        json!({"permission_overwrites": [{"id": "500", "type": 1, "allow": 1024, "deny": "8"}]}),
        json!({"permission_overwrites": [{"id": "500", "type": 1, "allow": "1024"}]}),
    ] {
        assert!(
            member_overwrite(&bad_doc, "500").is_err(),
            "{bad_doc} must stay refused"
        );
    }
}
