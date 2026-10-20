//! Protected acceptance: successful HTTP history is evidence only as a whole,
//! canonical, strictly descending page inside the requested cursor and size.
//! Dropping validation, filtering bad rows, or returning remote content in an
//! error must fail these tests. All HTTP traffic stays on the REST double.

use super::{
    executor_for, message_row, AuditMirror, MirrorError, MockRest, ScriptedResponse, BOT, CHANNEL,
};
use serde_json::{json, Value};

async fn rejects(page: Value, before: Option<&str>, limit: u8) {
    let mock = MockRest::start(
        vec![ScriptedResponse::json(200, page)],
        ScriptedResponse::status(500),
    )
    .await;
    let result = executor_for(&mock)
        .channel_history(CHANNEL, before, limit)
        .await;
    let requests = mock.requests();
    mock.shutdown().await;
    assert_eq!(
        requests.len(),
        1,
        "invalid success must not be retried or posted"
    );
    assert_eq!(requests[0].method, "GET");
    let Err(MirrorError::Uncertain(reason)) = result else {
        panic!("invalid complete history page must be uncertain, got {result:?}");
    };
    assert!(
        !reason.contains("history-content-sentinel"),
        "raw history content leaked"
    );
    assert!(
        !reason.contains("invalid-identity-sentinel"),
        "raw identity leaked"
    );
}

macro_rules! invalid_identity {
    ($name:ident, $field:literal, $value:expr) => {
        #[tokio::test]
        async fn $name() {
            let mut row = message_row("300", BOT, "history-content-sentinel");
            if $field == "message" {
                row["id"] = json!($value);
            } else {
                row["author"]["id"] = json!($value);
            }
            rejects(json!([row]), None, 100).await;
        }
    };
}

invalid_identity!(zero_message, "message", "0");
invalid_identity!(leading_zero_message, "message", "0300");
invalid_identity!(signed_message, "message", "+300");
invalid_identity!(whitespace_message, "message", " 300");
invalid_identity!(overflow_message, "message", "18446744073709551616");
invalid_identity!(empty_message, "message", "");
invalid_identity!(numeric_message, "message", 300);
invalid_identity!(zero_author, "author", "0");
invalid_identity!(leading_zero_author, "author", "09999");
invalid_identity!(signed_author, "author", "+9999");
invalid_identity!(whitespace_author, "author", "9999 ");
invalid_identity!(overflow_author, "author", "18446744073709551616");
invalid_identity!(empty_author, "author", "");
invalid_identity!(nonnumeric_author, "author", "invalid-identity-sentinel");
invalid_identity!(numeric_author, "author", 9999);

#[tokio::test]
async fn malformed_author_rejects_every_position_without_filtering() {
    for index in [0, 49, 99] {
        for author in [Value::Null, json!([]), json!({}), json!({"id": null})] {
            let mut page: Vec<_> = (201..=300)
                .rev()
                .map(|id| message_row(&id.to_string(), BOT, "history-content-sentinel"))
                .collect();
            page[index]["author"] = author;
            rejects(json!(page), None, 100).await;
        }
    }
}

#[tokio::test]
async fn duplicate_ids_are_not_exhaustion() {
    rejects(
        json!([message_row("300", BOT, "x"), message_row("300", BOT, "x")]),
        None,
        100,
    )
    .await;
}

#[tokio::test]
async fn reversed_rows_are_not_a_boundary() {
    rejects(
        json!([message_row("250", BOT, "x"), message_row("300", BOT, "x")]),
        None,
        100,
    )
    .await;
}

#[tokio::test]
async fn interior_disorder_rejects_an_otherwise_descending_page() {
    rejects(
        json!([
            message_row("300", BOT, "x"),
            message_row("250", BOT, "x"),
            message_row("275", BOT, "x"),
            message_row("200", BOT, "x")
        ]),
        None,
        100,
    )
    .await;
}

#[tokio::test]
async fn oversized_page_is_not_truncated() {
    rejects(
        json!([
            message_row("300", BOT, "x"),
            message_row("250", BOT, "x"),
            message_row("200", BOT, "x")
        ]),
        None,
        2,
    )
    .await;
}

#[tokio::test]
async fn equal_cursor_is_not_progress() {
    rejects(
        json!([message_row("300", BOT, "x"), message_row("250", BOT, "x")]),
        Some("300"),
        100,
    )
    .await;
}

#[tokio::test]
async fn newer_than_cursor_is_not_progress() {
    rejects(
        json!([message_row("301", BOT, "x"), message_row("250", BOT, "x")]),
        Some("300"),
        100,
    )
    .await;
}

#[tokio::test]
async fn valid_numeric_order_empty_short_and_full_pages_remain_usable() {
    for (page, before, limit, ids) in [
        (json!([]), None, 100, vec![]),
        (
            json!([
                message_row("10", "18446744073709551615", ""),
                message_row("9", BOT, "x")
            ]),
            Some("11"),
            2,
            vec!["10", "9"],
        ),
        (
            json!([
                message_row("18446744073709551615", BOT, "x"),
                message_row("1", BOT, "x")
            ]),
            None,
            100,
            vec!["18446744073709551615", "1"],
        ),
    ] {
        let mock = MockRest::start(
            vec![ScriptedResponse::json(200, page)],
            ScriptedResponse::status(500),
        )
        .await;
        let result = executor_for(&mock)
            .channel_history(CHANNEL, before, limit)
            .await;
        let requests = mock.requests();
        mock.shutdown().await;
        let rows = result.expect("valid complete evidence remains usable");
        assert_eq!(
            rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
            ids
        );
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "GET");
        assert!(requests[0].path.contains(&format!("limit={limit}")));
        if let Some(cursor) = before {
            assert!(requests[0].path.contains(&format!("before={cursor}")));
        }
    }
}
