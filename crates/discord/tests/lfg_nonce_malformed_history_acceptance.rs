//! Acceptance tests: a nonce-history scan may only report "absent" from
//! rows it could actually read. A malformed row (missing or mistyped
//! message identity, author, or target-channel evidence) makes acceptance
//! uncertain, never absent — so an ambiguously posted LFG message is
//! retained instead of cleaned up.
//!
//! Direct tests drive `ActionExecutor::recover_message_by_nonce` against
//! the scripted MockRest seam. Routed tests (feature `db`, disposable test
//! database only) drive the real shared interaction router.

#[allow(dead_code)]
mod common;

use common::{MockRest, ScriptedResponse};
use serde_json::{json, Value};
use two_bot_discord::{ActionExecutor, DiscordError};

const CHANNEL: &str = "4444";
const OTHER_CHANNEL: &str = "9999";
const BOT: u64 = 1111;
const OTHER_AUTHOR: u64 = 3333;
fn nonce() -> String {
    static NONCE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    NONCE
        .get_or_init(|| {
            let value = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or_else(|_| std::process::id() as u128);
            format!("{value:032x}")
        })
        .clone()
}
/// Planted in hostile rows; the fixed uncertainty error must never echo it.
const RAW_SENTINEL: &str = "raw-upstream-sentinel-7f3a";

fn executor_for(mock: &MockRest) -> ActionExecutor {
    ActionExecutor::with_proxy("nonce-history-token".to_owned(), Some(mock.origin()))
        .expect("executor builds against the mock")
}

/// A structurally complete history row in the target channel. The nonce
/// field is omitted unless `nonce` is given; Discord omits it for messages
/// sent without one, and that must not make the row unreadable.
fn valid_row(id: u64, author: u64, nonce: Option<&str>) -> Value {
    let mut row = json!({
        "id": id.to_string(),
        "channel_id": CHANNEL,
        "author": {"id": author.to_string()},
    });
    if let Some(nonce) = nonce {
        row["nonce"] = json!(nonce);
    }
    row
}

/// `count` valid non-matching rows with descending ids starting at `top`.
fn valid_page(top: u64, count: u64) -> Value {
    Value::Array(
        (0..count)
            .map(|offset| valid_row(top - offset, OTHER_AUTHOR, None))
            .collect(),
    )
}

fn history_gets(mock: &MockRest) -> Vec<String> {
    mock.requests()
        .into_iter()
        .filter(|request| request.method == "GET")
        .map(|request| request.path)
        .collect()
}

async fn recover(
    script: Vec<ScriptedResponse>,
) -> (Result<Option<String>, DiscordError>, MockRest) {
    let mock = MockRest::start(script, ScriptedResponse::status(500)).await;
    let result = executor_for(&mock)
        .recover_message_by_nonce(CHANNEL, &nonce(), BOT)
        .await;
    (result, mock)
}

fn assert_unavailable_without_raw_echo(result: &Result<Option<String>, DiscordError>, case: &str) {
    let err = match result {
        Err(DiscordError::Unavailable(message)) => message.clone(),
        other => panic!("{case}: expected Err(DiscordError::Unavailable), got {other:?}"),
    };
    assert!(
        !err.contains(RAW_SENTINEL),
        "{case}: error must not echo raw upstream row data, got {err:?}"
    );
}

fn noncanonical_identity_row(message_id: &str, author_id: &str) -> Value {
    let mut row = valid_row(5002, BOT, Some(&nonce()));
    row["id"] = json!(message_id);
    row["author"]["id"] = json!(author_id);
    row
}

fn bot_nonce_row(nonce: Value) -> Value {
    let mut row = valid_row(5002, BOT, None);
    row["nonce"] = nonce;
    row
}

async fn assert_review_row_is_unreadable(mut row: Value, later_page: bool) {
    row["content"] = json!(RAW_SENTINEL);
    let mut history = Vec::new();
    if later_page {
        history.push(ScriptedResponse::json(200, valid_page(10_000, 100)));
    }
    history.push(ScriptedResponse::json(200, json!([row])));
    let (result, mock) = recover(history).await;
    let gets = history_gets(&mock);
    mock.shutdown().await;
    assert_eq!(gets.len(), if later_page { 2 } else { 1 });
    if later_page {
        assert!(gets[1].contains("before=9901"));
    }
    assert_unavailable_without_raw_echo(&result, "unreadable review row");
    match result {
        Err(DiscordError::Unavailable(message)) => {
            assert_eq!(message, "unreadable message history");
        }
        other => panic!("expected fixed unreadable-history error, got {other:?}"),
    }
}

// Separate tests keep every malformed form observable in the baseline RED run,
// including rows reached only after a full, readable first page.
macro_rules! unreadable_on_first_and_later_page {
    ($first:ident, $later:ident, $row:expr) => {
        #[tokio::test]
        async fn $first() {
            assert_review_row_is_unreadable($row, false).await;
        }

        #[tokio::test]
        async fn $later() {
            assert_review_row_is_unreadable($row, true).await;
        }
    };
}

unreadable_on_first_and_later_page!(
    leading_zero_author_is_unreadable_on_first_page,
    leading_zero_author_is_unreadable_on_later_page,
    noncanonical_identity_row("5002", "01111")
);
unreadable_on_first_and_later_page!(
    plus_author_is_unreadable_on_first_page,
    plus_author_is_unreadable_on_later_page,
    noncanonical_identity_row("5002", "+1111")
);
unreadable_on_first_and_later_page!(
    leading_zero_message_is_unreadable_on_first_page,
    leading_zero_message_is_unreadable_on_later_page,
    noncanonical_identity_row("05002", "1111")
);
unreadable_on_first_and_later_page!(
    plus_message_is_unreadable_on_first_page,
    plus_message_is_unreadable_on_later_page,
    noncanonical_identity_row("+5002", "1111")
);
unreadable_on_first_and_later_page!(
    boolean_bot_nonce_is_unreadable_on_first_page,
    boolean_bot_nonce_is_unreadable_on_later_page,
    bot_nonce_row(json!(true))
);
unreadable_on_first_and_later_page!(
    object_bot_nonce_is_unreadable_on_first_page,
    object_bot_nonce_is_unreadable_on_later_page,
    bot_nonce_row(json!({}))
);
unreadable_on_first_and_later_page!(
    array_bot_nonce_is_unreadable_on_first_page,
    array_bot_nonce_is_unreadable_on_later_page,
    bot_nonce_row(json!([]))
);
unreadable_on_first_and_later_page!(
    fractional_bot_nonce_is_unreadable_on_first_page,
    fractional_bot_nonce_is_unreadable_on_later_page,
    bot_nonce_row(json!(1.5))
);

#[tokio::test]
async fn readable_optional_bot_nonces_and_foreign_malformed_nonces_prove_absence() {
    let mut rows = vec![valid_row(5100, BOT, None)];
    for nonce in [
        Value::Null,
        json!(0),
        json!(42),
        json!(-42),
        json!(u64::MAX),
        json!(""),
        json!("readable-nonmatching-nonce"),
    ] {
        rows.push(bot_nonce_row(nonce));
    }
    for nonce in [json!(true), json!({}), json!([]), json!(1.5)] {
        let mut row = valid_row(5101, OTHER_AUTHOR, None);
        row["nonce"] = nonce;
        rows.push(row);
    }
    for later_page in [false, true] {
        let mut history = Vec::new();
        if later_page {
            history.push(ScriptedResponse::json(200, valid_page(10_000, 100)));
        }
        history.push(ScriptedResponse::json(200, json!(rows)));
        let (result, mock) = recover(history).await;
        let gets = history_gets(&mock);
        mock.shutdown().await;
        assert_eq!(result.expect("readable nonmatching history"), None);
        assert_eq!(gets.len(), if later_page { 2 } else { 1 });
        if later_page {
            assert!(gets[1].contains("before=9901"));
        }
    }
}

#[tokio::test]
async fn malformed_rows_on_first_short_page_are_unavailable_not_absent() {
    let cases: Vec<(&str, Value)> = vec![
        ("null row", json!([null])),
        ("empty object row", json!([{}])),
        ("string row", json!([RAW_SENTINEL])),
        (
            "missing message id",
            json!([{"channel_id": CHANNEL, "author": {"id": BOT.to_string()},
                    "content": RAW_SENTINEL}]),
        ),
        (
            "numeric message id",
            json!([{"id": 5001, "channel_id": CHANNEL, "author": {"id": BOT.to_string()}}]),
        ),
        (
            "empty message id",
            json!([{"id": "", "channel_id": CHANNEL, "author": {"id": BOT.to_string()}}]),
        ),
        (
            "missing author",
            json!([{"id": "5001", "channel_id": CHANNEL, "content": RAW_SENTINEL}]),
        ),
        (
            "null author",
            json!([{"id": "5001", "channel_id": CHANNEL, "author": null}]),
        ),
        (
            "author not an object",
            json!([{"id": "5001", "channel_id": CHANNEL, "author": RAW_SENTINEL}]),
        ),
        (
            "author missing id",
            json!([{"id": "5001", "channel_id": CHANNEL, "author": {}}]),
        ),
        (
            "numeric author id",
            json!([{"id": "5001", "channel_id": CHANNEL, "author": {"id": 1111}}]),
        ),
        (
            "empty author id",
            json!([{"id": "5001", "channel_id": CHANNEL, "author": {"id": ""}}]),
        ),
        (
            "missing channel id",
            json!([{"id": "5001", "author": {"id": BOT.to_string()},
                    "content": RAW_SENTINEL}]),
        ),
        (
            "numeric channel id",
            json!([{"id": "5001", "channel_id": 4444, "author": {"id": BOT.to_string()}}]),
        ),
        (
            "empty channel id",
            json!([{"id": "5001", "channel_id": "", "author": {"id": BOT.to_string()}}]),
        ),
        (
            "row from a different channel",
            json!([{"id": "5001", "channel_id": OTHER_CHANNEL,
                    "author": {"id": BOT.to_string()}}]),
        ),
        (
            "malformed row after a valid non-matching row",
            json!([valid_row(5002, OTHER_AUTHOR, None), {}]),
        ),
    ];
    for (case, page) in cases {
        let (result, mock) = recover(vec![ScriptedResponse::json(200, page)]).await;
        assert_unavailable_without_raw_echo(&result, case);
        assert_eq!(
            history_gets(&mock).len(),
            1,
            "{case}: a malformed short page is decided on that page, not paged past"
        );
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn malformed_row_on_later_page_is_unavailable_not_absent() {
    for (case, second_page) in [
        ("empty object on page two", json!([{}])),
        ("null on page two", json!([null])),
        (
            "missing author on page two",
            json!([{"id": "5001", "channel_id": CHANNEL, "content": RAW_SENTINEL}]),
        ),
        (
            "different channel on page two",
            json!([{"id": "5001", "channel_id": OTHER_CHANNEL,
                    "author": {"id": BOT.to_string()}}]),
        ),
    ] {
        // A full first page (100 valid rows, ids 10000..=9901) forces a
        // second scan; the malformed evidence sits on that later page.
        let (result, mock) = recover(vec![
            ScriptedResponse::json(200, valid_page(10_000, 100)),
            ScriptedResponse::json(200, second_page),
        ])
        .await;
        assert_unavailable_without_raw_echo(&result, case);
        let gets = history_gets(&mock);
        assert_eq!(gets.len(), 2, "{case}: both pages are scanned");
        assert!(
            gets[1].contains("before=9901"),
            "{case}: second page continues from the first page's last id, got {}",
            gets[1]
        );
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn valid_rows_without_a_nonce_still_prove_absence_on_a_short_page() {
    // Compatibility control: an absent (or null) nonce on an otherwise
    // valid row is ordinary history, not malformed evidence.
    let mut null_nonce = valid_row(5003, OTHER_AUTHOR, None);
    null_nonce["nonce"] = Value::Null;
    let page = json!([
        valid_row(5001, BOT, None),
        valid_row(5002, OTHER_AUTHOR, None),
        null_nonce,
        valid_row(5004, BOT, Some("ffffffffffffffffffffffff")),
        valid_row(5005, OTHER_AUTHOR, Some(&nonce())),
    ]);
    let (result, mock) = recover(vec![ScriptedResponse::json(200, page)]).await;
    assert_eq!(result.expect("readable history"), None);
    assert_eq!(history_gets(&mock).len(), 1);
    mock.shutdown().await;
}

#[tokio::test]
async fn valid_empty_history_still_proves_absence() {
    let (result, mock) = recover(vec![ScriptedResponse::json(200, json!([]))]).await;
    assert_eq!(result.expect("readable empty history"), None);
    assert_eq!(history_gets(&mock).len(), 1);
    mock.shutdown().await;
}

#[tokio::test]
async fn valid_own_nonce_match_still_recovers_without_resending() {
    // A foreign author's copy of the nonce never establishes acceptance;
    // the bot's own row in the target channel does.
    let page = json!([
        valid_row(5001, OTHER_AUTHOR, Some(&nonce())),
        valid_row(5002, BOT, Some(&nonce())),
    ]);
    let (result, mock) = recover(vec![ScriptedResponse::json(200, page)]).await;
    assert_eq!(result.expect("recovery"), Some("5002".to_owned()));
    assert_eq!(history_gets(&mock).len(), 1);
    mock.shutdown().await;
}

#[tokio::test]
async fn valid_own_nonce_match_on_later_page_still_recovers() {
    let (result, mock) = recover(vec![
        ScriptedResponse::json(200, valid_page(10_000, 100)),
        ScriptedResponse::json(200, json!([valid_row(5002, BOT, Some(&nonce()))])),
    ])
    .await;
    assert_eq!(result.expect("recovery"), Some("5002".to_owned()));
    assert_eq!(history_gets(&mock).len(), 2);
    mock.shutdown().await;
}

#[tokio::test]
async fn exhausted_three_full_valid_pages_remain_uncertain() {
    let (result, mock) = recover(vec![
        ScriptedResponse::json(200, valid_page(30_000, 100)),
        ScriptedResponse::json(200, valid_page(20_000, 100)),
        ScriptedResponse::json(200, valid_page(10_000, 100)),
    ])
    .await;
    assert!(
        result.is_err(),
        "an exhausted bounded scan proves neither acceptance nor absence, got {result:?}"
    );
    assert_eq!(history_gets(&mock).len(), 3, "the three-page bound holds");
    mock.shutdown().await;
}

#[tokio::test]
async fn denied_history_and_missing_bot_identity_remain_uncertain() {
    let (result, mock) = recover(vec![ScriptedResponse::status(403)]).await;
    assert!(result.is_err(), "denied history is not empty history");
    mock.shutdown().await;

    let mock = MockRest::start(vec![], ScriptedResponse::json(200, json!([]))).await;
    let result = executor_for(&mock)
        .recover_message_by_nonce(CHANNEL, &nonce(), 0)
        .await;
    assert!(result.is_err(), "unknown bot identity proves nothing");
    assert!(
        mock.requests().is_empty(),
        "no history is read against a guessed author"
    );
    mock.shutdown().await;
}

/// Routed disposable-DB tests: the same invariant at the served LFG seam.
#[cfg(feature = "db")]
mod routed {
    use super::*;
    use twilight_model::application::interaction::Interaction;
    use two_bot_core::{commands::PERM_MANAGE_EVENTS, lfg, lfg_store as store, RouterGates};
    use two_bot_discord::interactions::InteractionRuntime;

    const GUILD: &str = "2222";

    fn gates() -> RouterGates {
        RouterGates {
            configured_guild: Some(2222),
            announcements: true,
            scorecard: false,
            automations: false,
            moderation: false,
            voice: false,
            voice_assistant: false,
            tickets: false,
            self_roles: false,
            onboarding_picker: false,
            session_picker: false,
        }
    }

    fn create(id: u64) -> Interaction {
        serde_json::from_value(json!({
            "id": id.to_string(), "application_id": BOT.to_string(), "type": 2,
            "guild_id": GUILD, "channel": {"id": CHANNEL, "type": 0},
            "member": {"user": {"id": "3333", "username": "member", "discriminator": "0", "avatar": null},
                "roles": [], "joined_at": null, "deaf": false, "mute": false, "flags": 0,
                "permissions": PERM_MANAGE_EVENTS.to_string()},
            "token": "lfg-mock-token", "version": 1, "entitlements": [],
            "authorizing_integration_owners": {},
            "data": {"id": "1", "name": "lfg", "type": 1, "options": [
                {"type": 3, "name": "title", "value": "Friday raid"},
                {"type": 3, "name": "starts-at", "value": "2099-09-11T20:00:00Z"},
                {"type": 3, "name": "roles", "value": "tank:Tank:1,dps:DPS:1"}
            ]}
        })).expect("wire interaction")
    }

    fn runtime(pool: sqlx::PgPool, mock: &MockRest) -> InteractionRuntime {
        InteractionRuntime::new(
            gates(),
            pool,
            ActionExecutor::with_proxy("lfg-test-token".into(), Some(mock.origin()))
                .expect("executor"),
            BOT,
            two_bot_core::ClassifierConfig::default(),
        )
    }

    async fn connect_schema_pool(url: &str, schema: &str) -> sqlx::PgPool {
        let search = schema.to_owned();
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(5)
            .after_connect(move |conn, _| {
                let search = search.clone();
                Box::pin(async move {
                    sqlx::query("SELECT set_config('search_path', $1, false)")
                        .bind(search)
                        .execute(conn)
                        .await?;
                    Ok(())
                })
            })
            .connect(url)
            .await
            .expect("schema pool")
    }

    struct TestDb {
        pool: sqlx::PgPool,
        admin: sqlx::PgPool,
        schema: String,
    }
    impl TestDb {
        async fn new() -> Self {
            // Fixed agent test endpoint or this job's disposable CI service
            // only. Never application URLs or inherited credentials.
            let url = if std::env::var("TWO_LFG_TESTDB_CI").as_deref() == Ok("1") {
                "postgres://agent_test@127.0.0.1:5432/agent_test"
            } else {
                "postgres://agent_test@agent-testdb:5432/two_bot_test_tog10084"
            };
            let admin = sqlx::PgPool::connect(url).await.expect("testdb reachable");
            let schema = format!(
                "lfg_nonce_history_{}",
                time::OffsetDateTime::now_utc().unix_timestamp_nanos()
            );
            // Audited identifier: constant prefix + timestamp digits only.
            sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
                .execute(&admin)
                .await
                .expect("test schema");
            let pool = connect_schema_pool(url, &schema).await;
            sqlx::migrate!("../cutover/migrations")
                .run(&pool)
                .await
                .expect("migrations");
            Self {
                pool,
                admin,
                schema,
            }
        }
        async fn cleanup(self) {
            self.pool.close().await;
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "DROP SCHEMA {} CASCADE",
                self.schema
            )))
            .execute(&self.admin)
            .await
            .expect("cleanup test schema");
            self.admin.close().await;
        }
    }

    fn body(request: &common::RestRequest) -> Value {
        serde_json::from_slice(&request.body).expect("JSON body")
    }

    fn last_reply(mock: &MockRest) -> String {
        let requests = mock.requests();
        body(requests.last().expect("reply"))["content"]
            .as_str()
            .expect("content")
            .into()
    }

    fn channel_posts(mock: &MockRest) -> usize {
        mock.requests()
            .iter()
            .filter(|r| r.method == "POST" && r.path.starts_with("/api/v10/channels/"))
            .count()
    }

    /// One ambiguous create: callback 204, message POST fails ambiguously
    /// (500), then the scripted history pages decide the outcome.
    async fn ambiguous_create(
        pool: &sqlx::PgPool,
        id: u64,
        history: Vec<ScriptedResponse>,
    ) -> MockRest {
        let mut script = vec![ScriptedResponse::status(204), ScriptedResponse::status(500)];
        script.extend(history);
        let mock =
            MockRest::start(script, ScriptedResponse::json(200, json!({"id": "5900"}))).await;
        let rt = runtime(pool.clone(), &mock);
        rt.handle(&create(id)).await.unwrap();
        drop(rt);
        mock
    }

    async fn assert_review_row_retains_post(id: u64, mut row: Value, later_page: bool) {
        let db = TestDb::new().await;
        row["content"] = json!(RAW_SENTINEL);
        let mut history = Vec::new();
        if later_page {
            history.push(ScriptedResponse::json(200, valid_page(10_000, 100)));
        }
        history.push(ScriptedResponse::json(200, json!([row])));
        let mock = ambiguous_create(&db.pool, id, history).await;
        let post = format!("lfg-{id}");
        let stored = store::get_lfg(&db.pool, GUILD, &post).await.unwrap();
        let roles = store::list_lfg_roles(&db.pool, &post).await.unwrap();
        let reply = last_reply(&mock);
        let gets = history_gets(&mock);
        let posts = channel_posts(&mock);
        let requests = mock.requests();
        mock.shutdown().await;
        db.cleanup().await;

        // Clean the disposable fixture even when the baseline deletes the post.
        let stored = stored.expect("unreadable nonce evidence must retain the uncertain post");
        assert!(
            stored.message_id.is_none(),
            "no recovered message is invented"
        );
        assert_eq!(
            roles.len(),
            2,
            "the retained post keeps both configured roles"
        );
        assert!(reply.contains("acceptance is uncertain"), "got {reply:?}");
        assert!(
            !reply.contains(RAW_SENTINEL),
            "no upstream content in replies"
        );
        assert_eq!(posts, 1, "no replacement channel POST");
        assert_eq!(gets.len(), if later_page { 2 } else { 1 });
        if later_page {
            assert!(gets[1].contains("before=9901"));
        }
        let sent = requests
            .iter()
            .find(|request| {
                request.method == "POST" && request.path.starts_with("/api/v10/channels/")
            })
            .expect("the ambiguous original channel POST was sent");
        assert_eq!(body(sent)["nonce"], json!(lfg::lfg_nonce(&post)));
        assert!(requests.iter().all(|request| request.method != "DELETE"));
    }

    async fn assert_noncanonical_identity_retention(later_page: bool) {
        for (offset, (message_id, author_id)) in [
            ("5002", "01111"),
            ("5002", "+1111"),
            ("05002", "1111"),
            ("+5002", "1111"),
        ]
        .into_iter()
        .enumerate()
        {
            let id = 7700 + offset as u64;
            let mut row = noncanonical_identity_row(message_id, author_id);
            row["nonce"] = json!(lfg::lfg_nonce(&format!("lfg-{id}")));
            assert_review_row_retains_post(id, row, later_page).await;
        }
    }

    async fn assert_malformed_bot_nonce_retention(later_page: bool) {
        for (offset, nonce) in [json!(true), json!({}), json!([]), json!(1.5)]
            .into_iter()
            .enumerate()
        {
            assert_review_row_retains_post(7710 + offset as u64, bot_nonce_row(nonce), later_page)
                .await;
        }
    }

    #[tokio::test]
    #[ignore = "needs agent-testdb or the CI service container"]
    async fn router_retains_noncanonical_identity_on_first_short_page() {
        assert_noncanonical_identity_retention(false).await;
    }

    #[tokio::test]
    #[ignore = "needs agent-testdb or the CI service container"]
    async fn router_retains_noncanonical_identity_on_later_page() {
        assert_noncanonical_identity_retention(true).await;
    }

    #[tokio::test]
    #[ignore = "needs agent-testdb or the CI service container"]
    async fn router_retains_malformed_bot_nonce_on_first_short_page() {
        assert_malformed_bot_nonce_retention(false).await;
    }

    #[tokio::test]
    #[ignore = "needs agent-testdb or the CI service container"]
    async fn router_retains_malformed_bot_nonce_on_later_page() {
        assert_malformed_bot_nonce_retention(true).await;
    }

    #[tokio::test]
    #[ignore = "needs agent-testdb or the CI service container"]
    async fn router_retains_ambiguous_post_when_history_rows_are_malformed() {
        let db = TestDb::new().await;
        let cases: Vec<(u64, &str, Value)> = vec![
            (7601, "null row", json!([null])),
            (7602, "empty object row", json!([{}])),
            (
                7603,
                "missing author",
                json!([{"id": "5001", "channel_id": CHANNEL, "content": RAW_SENTINEL}]),
            ),
            (
                7604,
                "missing channel id",
                json!([{"id": "5001", "author": {"id": BOT.to_string()}}]),
            ),
            (
                7605,
                "row from a different channel",
                json!([{"id": "5001", "channel_id": OTHER_CHANNEL,
                        "author": {"id": BOT.to_string()}}]),
            ),
        ];
        for (id, case, page) in cases {
            let mock =
                ambiguous_create(&db.pool, id, vec![ScriptedResponse::json(200, page)]).await;
            let post = format!("lfg-{id}");
            let stored = store::get_lfg(&db.pool, GUILD, &post)
                .await
                .unwrap()
                .unwrap_or_else(|| panic!("{case}: uncertain post must be retained"));
            assert!(
                stored.message_id.is_none(),
                "{case}: uncertainty leaves message_id unset"
            );
            assert_eq!(
                store::list_lfg_roles(&db.pool, &post).await.unwrap().len(),
                2,
                "{case}: retained post keeps its roles"
            );
            assert!(
                last_reply(&mock).contains("acceptance is uncertain"),
                "{case}: the reply reports uncertainty, got {:?}",
                last_reply(&mock)
            );
            assert_eq!(
                channel_posts(&mock),
                1,
                "{case}: no replacement POST is attempted"
            );
            mock.shutdown().await;
        }
        db.cleanup().await;
    }

    #[tokio::test]
    #[ignore = "needs agent-testdb or the CI service container"]
    async fn router_retains_ambiguous_post_when_later_history_page_is_malformed() {
        let db = TestDb::new().await;
        let mock = ambiguous_create(
            &db.pool,
            7611,
            vec![
                ScriptedResponse::json(200, super::valid_page(10_000, 100)),
                ScriptedResponse::json(200, json!([{}])),
            ],
        )
        .await;
        let post = "lfg-7611";
        let stored = store::get_lfg(&db.pool, GUILD, post)
            .await
            .unwrap()
            .expect("uncertain post must be retained");
        assert!(stored.message_id.is_none());
        assert_eq!(
            store::list_lfg_roles(&db.pool, post).await.unwrap().len(),
            2
        );
        assert!(last_reply(&mock).contains("acceptance is uncertain"));
        assert_eq!(channel_posts(&mock), 1, "no replacement POST");
        assert_eq!(history_gets(&mock).len(), 2, "both pages are scanned");
        mock.shutdown().await;
        db.cleanup().await;
    }

    #[tokio::test]
    #[ignore = "needs agent-testdb or the CI service container"]
    async fn router_still_cleans_up_when_valid_history_without_nonce_proves_absence() {
        // Compatibility control: readable rows that merely omit the
        // optional nonce still prove absence, so cleanup is unchanged.
        let db = TestDb::new().await;
        let post = "lfg-7621";
        assert_ne!(lfg::lfg_nonce(post), nonce(), "control uses the real nonce");
        let page = json!([
            valid_row(5001, BOT, None),
            valid_row(5002, OTHER_AUTHOR, Some(&lfg::lfg_nonce(post))),
        ]);
        let mock = ambiguous_create(&db.pool, 7621, vec![ScriptedResponse::json(200, page)]).await;
        assert!(
            store::get_lfg(&db.pool, GUILD, post)
                .await
                .unwrap()
                .is_none(),
            "proven-absent post is cleaned up"
        );
        assert!(store::list_lfg_roles(&db.pool, post)
            .await
            .unwrap()
            .is_empty());
        assert!(last_reply(&mock).contains("operation failed"));
        assert_eq!(channel_posts(&mock), 1);
        mock.shutdown().await;
        db.cleanup().await;
    }
}
