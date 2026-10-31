//! Protected acceptance: catch-up never persists an unknown bot flag as human.

#[path = "../../core/tests/support/tracing_capture.rs"]
mod tracing_capture;

use serde_json::{json, Value};
use std::collections::HashSet;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use twilight_model::id::Id;
use two_bot_cutover::{
    member_catchup::{apply_member_page, fetch_member_page, MemberCatchupPage},
    rest::RestClient,
};
use two_bot_testsupport::TestDatabase;

const JOINED: &str = "2024-01-01T00:00:00.000000+00:00";
static DIAGNOSTICS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

// Discord omits `bot` for humans (`bot?` is optional, never null), so an absent
// key keeps the documented human default; only a present non-boolean value is
// unverifiable.
fn invalid_flags() -> Vec<(&'static str, Value)> {
    vec![
        ("null", Value::Null),
        ("string_false", json!("false")),
        ("string_true", json!("true")),
        ("zero", json!(0)),
        ("one", json!(1)),
        ("array", json!([])),
        ("object", json!({})),
    ]
}

fn member(id: &str, flag: Option<Value>, pending: Value) -> Value {
    let mut user = json!({"id": id, "username": "fixture", "discriminator": "0", "avatar": null});
    if let Some(flag) = flag {
        user["bot"] = flag;
    }
    json!({"user": user, "roles": [], "joined_at": JOINED, "pending": pending,
        "deaf": false, "mute": false, "flags": 0})
}

// Only Discord's external HTTP boundary is doubled; parsing, classification
// evidence and persistence below run through the real public catch-up APIs.
async fn fetch(rows: Vec<Value>) -> MemberCatchupPage {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback member fixture");
    let origin = format!("http://{}", listener.local_addr().expect("fixture address"));
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("member request");
        let mut request = Vec::new();
        let mut chunk = [0_u8; 1024];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let read = socket.read(&mut chunk).await.expect("request headers");
            assert!(read > 0 && request.len() < 8192, "bounded HTTP request");
            request.extend_from_slice(&chunk[..read]);
        }
        let request = String::from_utf8(request).expect("ASCII request");
        assert!(request.starts_with("GET /api/v10/guilds/2222/members?"));
        let body = serde_json::to_vec(&rows).expect("page JSON");
        let head = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len(),
        );
        socket
            .write_all(head.as_bytes())
            .await
            .expect("response headers");
        socket.write_all(&body).await.expect("response body");
    });
    let rest = RestClient::with_proxy("fixture".to_owned(), Some(origin));
    let page = fetch_member_page(&rest, Id::new(2222), 100, None)
        .await
        .expect("mixed page must retain its verifiable rows");
    task.await.expect("HTTP fixture task");
    page
}

fn expected_valid_bytes() -> &'static [u8] {
    br#"[{"member_id":"3333","joined_at":"2024-01-01T00:00:00.000000+00:00","pending":false,"is_bot":false},{"member_id":"9999","joined_at":"2024-01-01T00:00:00.000000+00:00","pending":true,"is_bot":true}]"#
}

fn valid_members() -> Vec<Value> {
    vec![
        member("3333", Some(json!(false)), json!(false)),
        member("9999", Some(json!(true)), json!(true)),
    ]
}

#[tokio::test]
async fn malformed_flags_skip_with_a_reason_without_changing_valid_row_bytes() {
    let _serial = DIAGNOSTICS.lock().await;
    let capture = tracing_capture::global();
    let mut observed = Vec::new();
    for (name, flag) in invalid_flags() {
        let mut rows = valid_members();
        rows.insert(1, member("7777", Some(flag), Value::Null));
        let offset = capture.text().len();
        let page = fetch(rows).await;
        let diagnostics = capture.text()[offset..].to_owned();
        let reason = diagnostics.lines().any(|line| {
            let lower = line.to_ascii_lowercase();
            lower.contains("member_catchup")
                && lower.contains("reason=")
                && (lower.contains("bot") || lower.contains("flag"))
        });
        observed.push((
            name,
            serde_json::to_vec(&page.members).expect("serialize actual rows"),
            page.aggregate_count,
            page.next_after,
            reason,
        ));
    }
    assert!(
        observed.iter().all(|(_, bytes, count, cursor, reason)|
            bytes == expected_valid_bytes() && *count == 2 && cursor.as_deref() == Some("9999") && *reason),
        "each unknown flag must skip with a bounded bot-flag reason; valid rows/count/cursor stay intact: {observed:?}"
    );
}

#[tokio::test]
async fn explicit_boolean_flags_preserve_valid_member_rows_byte_for_byte() {
    let page = fetch(valid_members()).await;
    assert_eq!(
        serde_json::to_vec(&page.members).expect("serialize rows"),
        expected_valid_bytes()
    );
    assert_eq!(page.aggregate_count, 2);
    assert_eq!(page.next_after.as_deref(), Some("9999"));
}

#[tokio::test]
async fn absent_flag_keeps_discords_documented_human_default() {
    let page = fetch(vec![member("4444", None, json!(false))]).await;
    assert_eq!(
        serde_json::to_vec(&page.members).expect("serialize rows"),
        br#"[{"member_id":"4444","joined_at":"2024-01-01T00:00:00.000000+00:00","pending":false,"is_bot":false}]"#
    );
    assert_eq!(page.aggregate_count, 1);
    assert_eq!(page.next_after.as_deref(), Some("4444"));
}

#[tokio::test]
async fn mixed_page_persists_only_verified_members_and_preserves_valid_database_values() {
    let _serial = DIAGNOSTICS.lock().await;
    let url = match std::env::var("TWO_TEST_DATABASE_URL") {
        Ok(url) => url,
        Err(std::env::VarError::NotPresent) => {
            eprintln!("disposable DB acceptance not run: TWO_TEST_DATABASE_URL unset");
            return;
        }
        Err(_) => panic!("test database URL must be Unicode"),
    };
    let db = TestDatabase::create(&url, &sqlx::migrate!("./migrations"))
        .await
        .expect("guarded disposable database");
    let control = fetch(valid_members()).await;
    apply_member_page(db.pool(), "1111", &control, &HashSet::new())
        .await
        .expect("persist valid control page");
    let mut mixed = valid_members();
    for (index, (_, flag)) in invalid_flags().into_iter().enumerate() {
        mixed.insert(
            1,
            member(&(7000 + index).to_string(), Some(flag), Value::Null),
        );
    }
    let page = fetch(mixed).await;
    let report = apply_member_page(db.pool(), "2222", &page, &HashSet::new())
        .await
        .expect("persist surviving valid rows");
    let read = |guild: &'static str| {
        sqlx::query_as::<_, (String, String, Option<bool>, bool)>(
            "SELECT member_id, joined_at::text, pending, is_bot
             FROM member_catchup_members WHERE guild_id = $1 ORDER BY member_id",
        )
        .bind(guild)
        .fetch_all(db.pool())
    };
    let control_rows = read("1111").await.expect("control values");
    let actual_rows = read("2222").await.expect("mixed values");
    db.close().await.expect("drop owned disposable database");
    assert_eq!(
        report.aggregate_count, 2,
        "unknown flags do not inflate verified counts"
    );
    assert_eq!(
        report.per_user_rows, 2,
        "unknown flags never reach the store"
    );
    assert_eq!(
        actual_rows, control_rows,
        "every persisted valid field is unchanged"
    );
    assert_eq!(
        actual_rows,
        vec![
            (
                "3333".to_owned(),
                "2024-01-01 00:00:00+00".to_owned(),
                Some(false),
                false
            ),
            (
                "9999".to_owned(),
                "2024-01-01 00:00:00+00".to_owned(),
                Some(true),
                true
            ),
        ]
    );
}
