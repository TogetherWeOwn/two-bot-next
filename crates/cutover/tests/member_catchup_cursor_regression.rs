//! Builder regressions: skipped bot flags must not truncate member pagination.

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use twilight_model::id::Id;
use two_bot_cutover::{member_catchup::fetch_member_page, rest::RestClient};

fn member(id: &str, bot: Value) -> Value {
    json!({
        "user": {"id": id, "username": "fixture", "discriminator": "0", "avatar": null, "bot": bot},
        "roles": [], "joined_at": "2024-01-01T00:00:00.000000+00:00",
        "pending": false, "deaf": false, "mute": false, "flags": 0
    })
}

async fn assert_resume_past_skipped_rows(rows: Vec<Value>, retained_ids: &[&str]) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback member fixture");
    let origin = format!("http://{}", listener.local_addr().expect("fixture address"));
    let server = tokio::spawn(async move {
        let mut requests = Vec::new();
        for page in [rows, vec![member("9999", json!(false))], vec![]] {
            let (mut socket, _) = listener.accept().await.expect("member request");
            let mut request = Vec::new();
            let mut chunk = [0_u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = socket.read(&mut chunk).await.expect("request headers");
                assert!(read > 0 && request.len() < 8192, "bounded HTTP request");
                request.extend_from_slice(&chunk[..read]);
            }
            let request = String::from_utf8(request).expect("ASCII request");
            requests.push(request.lines().next().expect("request line").to_owned());
            let body = serde_json::to_vec(&page).expect("page JSON");
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len(),
            );
            socket.write_all(head.as_bytes()).await.expect("headers");
            socket.write_all(&body).await.expect("body");
        }
        requests
    });
    let rest = RestClient::with_proxy("fixture".to_owned(), Some(origin));
    let first = fetch_member_page(&rest, Id::new(2222), 2, None)
        .await
        .expect("first page");
    assert_eq!(
        first
            .members
            .iter()
            .map(|row| row.member_id.as_str())
            .collect::<Vec<_>>(),
        retained_ids,
        "only verified members survive"
    );
    assert_eq!(first.aggregate_count, retained_ids.len());
    assert_eq!(
        first.next_after.as_deref(),
        Some("7777"),
        "resume advances past the last raw member even when its flag is malformed"
    );

    let second = fetch_member_page(&rest, Id::new(2222), 2, first.next_after)
        .await
        .expect("page after skipped members");
    assert_eq!(second.members.len(), 1);
    assert_eq!(second.members[0].member_id, "9999");
    assert!(!second.members[0].is_bot);
    assert_eq!(second.aggregate_count, 1);
    assert_eq!(second.next_after.as_deref(), Some("9999"));

    let last = fetch_member_page(&rest, Id::new(2222), 2, second.next_after)
        .await
        .expect("empty terminator");
    assert!(last.members.is_empty());
    assert_eq!(last.aggregate_count, 0);
    assert_eq!(last.next_after, None, "only the empty page ends this scan");

    let requests = server.await.expect("fixture task");
    assert_eq!(rest.requests(), 3);
    assert_eq!(requests.len(), 3);
    for (request, after) in requests.iter().zip([None, Some("7777"), Some("9999")]) {
        let path = request.split_whitespace().nth(1).expect("request path");
        let (route, query) = path.split_once('?').expect("query parameters");
        assert_eq!(route, "/api/v10/guilds/2222/members");
        let params: Vec<_> = query.split('&').collect();
        assert!(params.contains(&"limit=2"));
        let actual_after = params.iter().find_map(|pair| pair.strip_prefix("after="));
        assert_eq!(
            actual_after, after,
            "returned cursor reaches the next wire request"
        );
    }
}

#[tokio::test]
async fn malformed_last_row_does_not_rewind_the_resume_cursor() {
    assert_resume_past_skipped_rows(
        vec![member("3333", json!(false)), member("7777", Value::Null)],
        &["3333"],
    )
    .await;
}

#[tokio::test]
async fn all_malformed_page_does_not_hide_later_valid_members() {
    assert_resume_past_skipped_rows(
        vec![member("3333", json!("false")), member("7777", json!(0))],
        &[],
    )
    .await;
}
