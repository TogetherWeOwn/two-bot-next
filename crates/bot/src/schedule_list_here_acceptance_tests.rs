//! Protected acceptance: /schedule-list here:true, through gateway dispatch,
//! the real SQL store/pager and loopback REST. No implementation belongs here.
//! Every DB fixture is disposable; missing/invalid configuration fails closed.
//!
//! Mutations caught: ignore/coerce/last-win `here`; trust a supplied channel;
//! filter a capped guild window in memory; omit a predicate from count/OFFSET;
//! read unbounded matching rows; lose tied ids/whole rows; clear delivery fences;
//! claim a complete/empty/out-of-range list after a failed count/window read.

#![cfg(test)]

use super::*;
use crate::hotpath_settings_test_support::queries::{assert_fetch_bound, Queries, Query};
use two_bot_testsupport::TestDatabase;

const OTHER_CHANNEL: &str = "4444";

async fn database() -> TestDatabase {
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("explicit disposable test bootstrap");
    TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("guarded disposable database; no application credential fallback")
}

async fn seed(pool: &PgPool, guild: &str, channel: &str, n: usize, long: bool) -> String {
    let id = if long {
        format!("a1b2c3d4e5f60718293a4b5c{n:08x}")
    } else {
        format!("row-{n:03}")
    };
    // Consecutive pairs tie; reversed insertion must not reverse their ids.
    let stamp = format!("2030-01-01T{:02}:{:02}:00.000Z", n / 120, (n / 2) % 60);
    sqlx::query(
        "INSERT INTO scheduled_messages
         (id, guild_id, channel_id, body, next_run_at, interval_seconds, enabled,
          last_run_at, last_message_id, created_by, created_at, updated_by, updated_at,
          claim_token, claimed_at, occurrence_nonce, delivery_uncertain)
         VALUES ($1, $2, $3, '@everyone PRIVATE-BODY', $4, $5, $6,
          '2029-12-31T23:00:00.000Z', '7777', '88', '2029-12-01T00:00:00.000Z',
          '88', '2029-12-01T00:00:00.000Z', 'fixture-claim',
          '2029-12-31T23:59:00.000Z', 'fixture-nonce', $7)",
    )
    .bind(&id)
    .bind(guild)
    .bind(channel)
    .bind(stamp)
    .bind((n.is_multiple_of(3)).then_some(5400_i64))
    .bind(n % 5 != 4)
    .bind(n.is_multiple_of(2))
    .execute(pool)
    .await
    .expect("seed schedules with live-shaped claims and both fence states");
    id
}

// Include every persisted field and xmin: no-op UPDATEs and delete/reinsert
// are writes too, even if their final values happen to be unchanged.
async fn state(pool: &PgPool, observed: bool) -> (Vec<(String, String)>, Vec<String>) {
    let query = if observed {
        "SELECT to_jsonb(s)::text, xmin::text FROM here_schedule_base s ORDER BY guild_id, id"
    } else {
        "SELECT to_jsonb(s)::text, xmin::text FROM scheduled_messages s ORDER BY guild_id, id"
    };
    let rows = sqlx::query_as(query)
        .fetch_all(pool)
        .await
        .expect("full schedule snapshot");
    let audits =
        sqlx::query_scalar("SELECT to_jsonb(a)::text FROM automation_audit_log a ORDER BY id")
            .fetch_all(pool)
            .await
            .expect("full audit snapshot");
    (rows, audits)
}

fn interaction(here: Option<bool>, page: Option<i64>, channel: Option<u64>) -> Interaction {
    let mut options = Vec::new();
    if let Some(page) = page {
        options.push(option("page", CommandOptionValue::Integer(page)));
    }
    if let Some(here) = here {
        options.push(option("here", CommandOptionValue::Boolean(here)));
    }
    slash("schedule-list", channel, options)
}

async fn dispatch(pool: PgPool, interaction: Interaction, enabled: bool) -> Vec<RestRequest> {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = CommandRuntime::new(
        pool,
        executor_at(origin),
        router_with_commands(gates(enabled, true)),
        GUILD,
        enabled,
    );
    runtime.set_identity(1111, 1111);
    let guard = runtime.dispatch_guard();
    assert!(
        runtime.dispatch(&interaction_event(interaction)),
        "gateway admits event"
    );
    wait_for_timeout(
        || (0..6).all(|lane| runtime.lane_in_flight(lane) == 0),
        "schedule-list gateway work settles",
        Duration::from_secs(10),
    )
    .await;
    let requests = mock.requests();
    drop(guard);
    drop(runtime);
    mock.shutdown().await;
    requests
}

fn reply(requests: &[RestRequest]) -> String {
    assert!(!requests.is_empty(), "a permitted invocation must answer");
    assert_eq!(requests[0].method, "POST");
    assert!(requests[0].path.ends_with("/callback"));
    let initial: serde_json::Value = serde_json::from_slice(&requests[0].body).expect("callback");
    assert_eq!(initial["data"]["flags"], 64, "private response");
    let body = if initial["type"] == 5 {
        assert_eq!(requests.len(), 2, "one defer + edit, no channel messages");
        assert_eq!(requests[1].method, "PATCH");
        assert!(requests[1].path.ends_with("/messages/@original"));
        serde_json::from_slice::<serde_json::Value>(&requests[1].body).expect("completion")
    } else {
        assert_eq!(initial["type"], 4, "immediate refusal");
        assert_eq!(requests.len(), 1, "refusal only");
        initial["data"].clone()
    };
    assert_eq!(body["allowed_mentions"]["parse"], serde_json::json!([]));
    let text = body["content"].as_str().expect("text response");
    assert!(
        text.encode_utf16().count() <= 2000,
        "whole reply budget: {text}"
    );
    assert!(!text.contains("PRIVATE-BODY") && !text.contains("@everyone"));
    text.to_owned()
}

fn ids(text: &str) -> Vec<&str> {
    schedule_list_row_lines(text)
        .into_iter()
        .map(schedule_list_row_id)
        .collect()
}

fn scoped(text: &str) {
    let lower = text.to_ascii_lowercase();
    assert!(
        lower.contains("this channel"),
        "name the active this-channel filter, including empty/out-of-range replies: {text}"
    );
}

// Page indicators drive traversal only. Independently seeded ids, complete row
// literals and SQL receipts below prove that packing cannot lose or invent rows.
async fn packed_pages(
    pool: &PgPool,
    here: Option<bool>,
    channel: u64,
    hidden: bool,
) -> (Vec<String>, i64) {
    let first = reply(
        &dispatch(
            pool.clone(),
            interaction(here, Some(1), Some(channel)),
            true,
        )
        .await,
    );
    let first_total = schedule_list_page_indicator(&first)
        .expect("localized 25-row window announces packed pages")
        .1;
    assert!((2..=25).contains(&first_total), "bounded first window");
    let mut texts = vec![first];
    for page in 2..=first_total {
        texts.push(reply(
            &dispatch(
                pool.clone(),
                interaction(here, Some(page), Some(channel)),
                true,
            )
            .await,
        ));
    }
    if hidden {
        let first_hidden = first_total + 1;
        let text = reply(
            &dispatch(
                pool.clone(),
                interaction(here, Some(first_hidden), Some(channel)),
                true,
            )
            .await,
        );
        let total = schedule_list_page_indicator(&text)
            .expect("full hidden window announces global packed pages")
            .1;
        assert!(
            (first_hidden..=first_total + 25).contains(&total),
            "bounded second window"
        );
        texts.push(text);
        for page in first_hidden + 1..=total {
            texts.push(reply(
                &dispatch(
                    pool.clone(),
                    interaction(here, Some(page), Some(channel)),
                    true,
                )
                .await,
            ));
        }
    }
    (texts, first_total)
}

fn assert_complete_row(line: &str, id: &str, channel: u64, n: usize) {
    let interval = if n.is_multiple_of(3) {
        " every 90m"
    } else {
        ""
    };
    let status = if n % 5 == 4 {
        " (disabled)".to_owned()
    } else {
        format!(
            " <t:{}:F>. The 15s ticker and delivery may be delayed.",
            1_893_456_000 + (n / 2) * 60
        )
    };
    assert_eq!(
        line,
        format!(
            "`{id}` `<#{channel}>` 2030-01-01T{:02}:{:02}:00.000Z{interval}{status}",
            n / 120,
            (n / 2) % 60
        ),
        "complete copyable row, including current localized due/uncertainty for enabled rows"
    );
}

fn assert_window_pages(texts: &[String], first_total: i64) {
    let total = texts.len() as i64;
    for (index, text) in texts.iter().enumerate() {
        let page = index as i64 + 1;
        assert_eq!(
            schedule_list_page_indicator(text),
            Some((
                page,
                if page <= first_total {
                    first_total
                } else {
                    total
                }
            )),
            "first-window and global hidden-window numbering: {text}"
        );
        assert!(!ids(text).is_empty(), "packed pages contain whole rows");
    }
    assert!(
        texts[first_total as usize - 1]
            .contains(&format!("Continued on page {}.", first_total + 1)),
        "terminal first-window page reaches the hidden window"
    );
    assert!(
        texts[first_total as usize - 1].contains("(truncated, 27 more)"),
        "matching count, not the number of unrendered packed rows"
    );
    assert!(
        texts
            .last()
            .expect("hidden page")
            .contains("(truncated, 2 more)"),
        "only rows beyond the bounded second window remain"
    );
}

// Observe executed SQLx receipts rather than a side-effecting view expression:
// PostgreSQL may evaluate a VOLATILE view column before the caller's outer
// ORDER BY/LIMIT. These receipts report the actual statement and rows returned
// to the store, so they can prove both scope and the materialized-row cap.
fn assert_scoped_schedule_reads(receipts: &[Query]) {
    let reads: Vec<_> = receipts
        .iter()
        .filter(|query| query.normalized().contains("from scheduled_messages"))
        .collect();
    assert!(
        !reads.is_empty(),
        "no executed-query receipt for the schedule read"
    );
    for query in reads {
        let sql = query.normalized();
        let predicate = sql
            .split_once(" where ")
            .expect("guild-scoped SQL WHERE")
            .1
            .split(" order by ")
            .next()
            .expect("predicate");
        assert!(
            predicate.contains("guild_id") && predicate.contains("channel_id"),
            "both filters belong in executed SQL: {sql}"
        );
    }
    assert_fetch_bound(
        receipts,
        |query| query.contains("from scheduled_messages"),
        25,
    );
}

#[tokio::test]
async fn schedule_here_omitted_false_and_existing_pages_are_byte_identical() {
    let db = database().await;
    seed(db.pool(), GUILD_S, CHANNEL_S, 0, false).await;
    seed(db.pool(), GUILD_S, OTHER_CHANNEL, 1, false).await;
    seed(db.pool(), "9999", CHANNEL_S, 2, false).await;
    let before = state(db.pool(), false).await;
    let mut replies = Vec::new();
    for here in [None, Some(false)] {
        for page in [None, Some(1), Some(0), Some(-1), Some(2), Some(i64::MAX)] {
            replies.push(
                dispatch(
                    db.pool().clone(),
                    interaction(here, page, Some(CHANNEL)),
                    true,
                )
                .await,
            );
        }
    }
    let after = state(db.pool(), false).await;
    db.close().await.expect("drop compatibility fixture");
    assert_eq!(
        before, after,
        "all valid and refused guild pages are read-only"
    );
    let text: Vec<_> = replies.iter().map(|r| reply(r)).collect();
    assert_eq!(
        &text[..6],
        &text[6..],
        "false is exactly the omitted guild list"
    );
    assert_eq!(text[0], text[1]);
    assert_eq!(
        text[0],
        "`row-000` `<#3333>` 2030-01-01T00:00:00.000Z every 90m <t:1893456000:F>. The 15s ticker and delivery may be delayed.\n\
         `row-001` `<#4444>` 2030-01-01T00:00:00.000Z <t:1893456000:F>. The 15s ticker and delivery may be delayed.",
        "omitted/false retain current localized due times and uncertainty, not the retired ISO-only output"
    );
    assert_eq!(text[2], "Page must be at least 1.");
    assert_eq!(text[3], "Page must be at least 1.");
    assert_eq!(
        text[4],
        "Page 2 is out of range: this server has 1 page(s) of schedules."
    );
    assert_eq!(
        text[5],
        format!(
            "Page {} is out of range: this server has 1 page(s) of schedules.",
            i64::MAX
        )
    );
}

#[tokio::test]
async fn schedule_here_uses_invoking_channel_and_preserves_claims() {
    let db = database().await;
    seed(db.pool(), GUILD_S, CHANNEL_S, 2, false).await;
    seed(db.pool(), GUILD_S, OTHER_CHANNEL, 1, false).await;
    seed(db.pool(), "9999", CHANNEL_S, 0, false).await;
    let before = state(db.pool(), false).await;
    let first = dispatch(
        db.pool().clone(),
        interaction(Some(true), None, Some(CHANNEL)),
        true,
    )
    .await;
    let other = dispatch(
        db.pool().clone(),
        interaction(Some(true), None, Some(4444)),
        true,
    )
    .await;
    let after = state(db.pool(), false).await;
    db.close().await.expect("drop invoking-channel fixture");
    assert_eq!(
        before, after,
        "no definition, fence, claim, nonce or audit mutation"
    );
    let first = reply(&first);
    let other = reply(&other);
    assert_eq!(
        ids(&first),
        ["row-002"],
        "guild AND invoking channel must scope rows"
    );
    assert_eq!(
        ids(&other),
        ["row-001"],
        "the actual invoking channel selects the filter"
    );
    scoped(&first);
    scoped(&other);
}

#[tokio::test]
async fn schedule_here_empty_channel_is_not_an_empty_server() {
    let db = database().await;
    seed(db.pool(), GUILD_S, OTHER_CHANNEL, 0, false).await;
    let before = state(db.pool(), false).await;
    let mut replies = Vec::new();
    for page in [None, Some(1), Some(2), Some(i64::MAX)] {
        replies.push(
            dispatch(
                db.pool().clone(),
                interaction(Some(true), page, Some(CHANNEL)),
                true,
            )
            .await,
        );
    }
    let after = state(db.pool(), false).await;
    db.close().await.expect("drop local-empty fixture");
    assert_eq!(before, after);
    let texts: Vec<_> = replies.iter().map(|r| reply(r)).collect();
    assert_eq!(texts[0], texts[1], "omitted page is page one");
    for text in &texts {
        assert!(
            ids(text).is_empty(),
            "local empty must not list other channels: {text}"
        );
        scoped(text);
        assert_ne!(
            text, "Nothing scheduled.",
            "do not claim the server is empty"
        );
    }
    assert!(
        texts[0].to_ascii_lowercase().contains("nothing")
            || texts[0].to_ascii_lowercase().contains("no schedules")
    );
    for text in &texts[2..] {
        assert!(
            text.contains("out of range") && text.contains("1 page(s)"),
            "one empty filtered page: {text}"
        );
    }
}

#[tokio::test]
async fn schedule_here_rejects_wrong_types_and_duplicates_before_reads() {
    let db = database().await;
    seed(db.pool(), GUILD_S, CHANNEL_S, 0, false).await;
    let before = state(db.pool(), false).await;
    // With the table temporarily unreadable, a parser refusal cannot be
    // confused with a list or a database-error reply.
    sqlx::query("ALTER TABLE scheduled_messages RENAME TO here_schedule_base")
        .execute(db.pool())
        .await
        .expect("hide store from malformed requests");
    let mut inputs = vec![
        vec![option(
            "here",
            CommandOptionValue::String("@everyone not-a-boolean".repeat(200)),
        )],
        vec![option("here", CommandOptionValue::Integer(1))],
        vec![option("here", CommandOptionValue::Number(1.0))],
        vec![option("here", CommandOptionValue::SubCommand(Vec::new()))],
    ];
    for (a, b) in [(true, false), (false, true), (true, true), (false, false)] {
        inputs.push(vec![
            option("here", CommandOptionValue::Boolean(a)),
            option("here", CommandOptionValue::Boolean(b)),
        ]);
    }
    let mut replies = Vec::new();
    for options in inputs {
        replies.push(
            dispatch(
                db.pool().clone(),
                slash("schedule-list", Some(CHANNEL), options),
                true,
            )
            .await,
        );
    }
    let after = state(db.pool(), true).await;
    db.close().await.expect("drop strict-decoding fixture");
    assert_eq!(before, after, "bad filters must not read/audit/mutate");
    for requests in replies {
        let text = reply(&requests);
        let lower = text.to_ascii_lowercase();
        assert!(
            text.encode_utf16().count() <= 300,
            "bounded correction: {text}"
        );
        assert!(
            lower.contains("here")
                && (lower.contains("boolean") || lower.contains("true") || lower.contains("once")),
            "explain the invalid filter usefully: {text}"
        );
        assert!(ids(&text).is_empty());
    }
}

#[tokio::test]
async fn schedule_here_never_takes_a_caller_supplied_channel_or_guild() {
    let db = database().await;
    seed(db.pool(), GUILD_S, CHANNEL_S, 0, false).await;
    seed(db.pool(), GUILD_S, OTHER_CHANNEL, 1, false).await;
    seed(db.pool(), "9999", CHANNEL_S, 2, false).await;
    let before = state(db.pool(), false).await;
    let request = slash(
        "schedule-list",
        Some(CHANNEL),
        vec![
            option("here", CommandOptionValue::Boolean(true)),
            option("channel", CommandOptionValue::Channel(Id::new(4444))),
            option("guild", CommandOptionValue::String("9999".to_owned())),
        ],
    );
    let result = dispatch(db.pool().clone(), request, true).await;
    let missing = dispatch(db.pool().clone(), interaction(Some(true), None, None), true).await;
    let after = state(db.pool(), false).await;
    db.close().await.expect("drop caller-scope fixture");
    assert_eq!(before, after);
    let text = reply(&result);
    // Unknown options may be rejected or ignored; they never control scope.
    assert!(
        !text.contains("row-001") && !text.contains("row-002"),
        "caller cannot redirect scope: {text}"
    );
    if !ids(&text).is_empty() {
        assert_eq!(ids(&text), ["row-000"]);
        scoped(&text);
    }
    let missing = reply(&missing);
    assert!(
        ids(&missing).is_empty() && missing.to_ascii_lowercase().contains("channel"),
        "missing channel fails closed: {missing}"
    );
}

#[tokio::test]
async fn schedule_here_preserves_permission_feature_and_guild_fences() {
    let db = database().await;
    seed(db.pool(), GUILD_S, CHANNEL_S, 0, false).await;
    let before = state(db.pool(), false).await;
    let mut no_permission = interaction(Some(true), None, Some(CHANNEL));
    no_permission.member.as_mut().expect("member").permissions = Some(Permissions::empty());
    let denied = dispatch(db.pool().clone(), no_permission, true).await;
    let disabled = dispatch(
        db.pool().clone(),
        interaction(Some(true), None, Some(CHANNEL)),
        false,
    )
    .await;
    let mut foreign = interaction(Some(true), None, Some(CHANNEL));
    foreign.guild_id = Some(Id::new(9999));
    let foreign = dispatch(db.pool().clone(), foreign, true).await;
    let mut foreign_app = interaction(Some(true), None, Some(CHANNEL));
    foreign_app.application_id = Id::new(9999);
    let foreign_app = dispatch(db.pool().clone(), foreign_app, true).await;
    let after = state(db.pool(), false).await;
    db.close().await.expect("drop policy fixture");
    assert_eq!(before, after);
    assert_eq!(reply(&denied), MANAGE_SERVER_REQUIRED);
    assert_eq!(reply(&disabled), AUTOMATIONS_DISABLED_REPLY);
    assert!(foreign.is_empty(), "foreign guild stays silent");
    assert!(foreign_app.is_empty(), "foreign application stays silent");
}

#[tokio::test]
async fn schedule_here_existing_guild_packed_windows_preserve_enriched_rows() {
    let db = database().await;
    for n in (0..52).rev() {
        seed(db.pool(), GUILD_S, CHANNEL_S, n, false).await;
    }
    seed(db.pool(), "9999", CHANNEL_S, 150, false).await;
    let before = state(db.pool(), false).await;
    let mut walks = Vec::new();
    for here in [None, Some(false)] {
        let (texts, first_total) = packed_pages(db.pool(), here, CHANNEL, true).await;
        let total = texts.len() as i64;
        let mut refusals = Vec::new();
        for page in [total + 1, i64::MAX] {
            refusals.push(reply(
                &dispatch(
                    db.pool().clone(),
                    interaction(here, Some(page), Some(CHANNEL)),
                    true,
                )
                .await,
            ));
        }
        walks.push((texts, first_total, refusals));
    }
    let after = state(db.pool(), false).await;
    db.close()
        .await
        .expect("drop unchanged packed-page control");
    assert_eq!(before, after);
    assert_eq!(
        walks[0], walks[1],
        "false is the current omitted packed list"
    );
    for (texts, first_total, refusals) in walks {
        assert_window_pages(&texts, first_total);
        for (pages, range) in [
            (&texts[..first_total as usize], 0..25),
            (&texts[first_total as usize..], 25..50),
        ] {
            assert_eq!(
                pages.iter().flat_map(|text| ids(text)).collect::<Vec<_>>(),
                range.map(|n| format!("row-{n:03}")).collect::<Vec<_>>()
            );
        }
        for (n, line) in texts
            .iter()
            .flat_map(|text| schedule_list_row_lines(text))
            .enumerate()
        {
            assert_complete_row(line, &format!("row-{n:03}"), CHANNEL, n);
        }
        for text in refusals {
            assert!(ids(&text).is_empty());
            assert!(
                text.contains("out of range") && text.contains(&format!("{} page(s)", texts.len())),
                "combined packed-page range stays honest: {text}"
            );
        }
    }
}

#[tokio::test]
async fn schedule_here_filters_sql_before_cap_offset_count_and_packing() {
    let db = database().await;
    // 30 earlier unrelated rows must not consume any of the matching cap.
    for n in (0..30).rev() {
        seed(db.pool(), GUILD_S, OTHER_CHANNEL, n, false).await;
    }
    // 52 matching rows interleaved with 52 unrelated rows; two bounded
    // matching windows must not consume unrelated rows at either offset.
    let mut expected = Vec::new();
    for n in (30usize..134).rev() {
        let channel = if n.is_multiple_of(2) {
            CHANNEL_S
        } else {
            OTHER_CHANNEL
        };
        let id = seed(db.pool(), GUILD_S, channel, n, false).await;
        if n.is_multiple_of(2) {
            expected.push(id);
        }
    }
    expected.reverse();
    seed(db.pool(), "9999", CHANNEL_S, 150, false).await;
    let before = state(db.pool(), false).await;
    let (texts, first_total) = packed_pages(db.pool(), Some(true), CHANNEL, true).await;
    let total = texts.len() as i64;
    let mut refusals = Vec::new();
    for page in [total + 1, i64::MAX] {
        refusals.push(
            dispatch(
                db.pool().clone(),
                interaction(Some(true), Some(page), Some(CHANNEL)),
                true,
            )
            .await,
        );
    }
    let after = state(db.pool(), false).await;
    db.close().await.expect("drop interleaved SQL fixture");
    assert_eq!(
        before, after,
        "successful filtered reads cannot audit or touch schedules"
    );
    let boundary = first_total as usize;
    assert_eq!(
        texts[..boundary]
            .iter()
            .flat_map(|text| ids(text))
            .collect::<Vec<_>>(),
        expected[..25],
        "all packed first-window pages expose 25 matching rows after unrelated prefix"
    );
    assert_eq!(
        texts[boundary..]
            .iter()
            .flat_map(|text| ids(text))
            .collect::<Vec<_>>(),
        expected[25..50],
        "OFFSET skips matching rows, not guild rows; global continuation loses none"
    );
    assert_window_pages(&texts, first_total);
    for text in &texts {
        scoped(text);
        for line in schedule_list_row_lines(text) {
            let id = schedule_list_row_id(line);
            let index = expected
                .iter()
                .position(|want| want == id)
                .expect("matching id");
            assert_complete_row(line, id, CHANNEL, 30 + index * 2);
        }
    }
    for requests in &refusals {
        let text = reply(requests);
        assert!(ids(&text).is_empty());
        assert!(
            text.contains("out of range") && text.contains(&format!("{total} page(s)")),
            "preserve the actual combined packed-page range: {text}"
        );
        scoped(&text);
    }
}

#[tokio::test]
async fn schedule_here_executed_store_reads_share_sql_scope_and_bounds() {
    let db = database().await;
    for n in 0..52 {
        seed(db.pool(), GUILD_S, CHANNEL_S, n, false).await;
    }
    for n in 52..82 {
        seed(db.pool(), GUILD_S, OTHER_CHANNEL, n, false).await;
    }
    let before = state(db.pool(), false).await;
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = CommandRuntime::new(
        db.pool().clone(),
        executor_at(origin),
        router_with_commands(gates(true, true)),
        GUILD,
        true,
    );
    // Capture the awaited public service boundary: the gateway case above
    // separately covers detached dispatch, whose task has its own subscriber.
    let ((first_total, total), receipts) = Queries::capture(async {
        runtime
            .on_interaction(&interaction(Some(true), Some(1), Some(CHANNEL)))
            .await;
        let first = reply(&mock.requests());
        let first_total = schedule_list_page_indicator(&first)
            .expect("packed first window")
            .1;
        assert!((2..=25).contains(&first_total));
        // Page 2 can still be inside the first fetch. Walk to the actual
        // first hidden page so the SQL capture must include OFFSET.
        for page in 2..=first_total + 1 {
            runtime
                .on_interaction(&interaction(Some(true), Some(page), Some(CHANNEL)))
                .await;
        }
        let contents = schedule_list_reply_contents(&mock, first_total as usize + 1);
        let total = schedule_list_page_indicator(contents.last().expect("hidden reply"))
            .expect("packed hidden window")
            .1;
        assert!((first_total + 1..=first_total + 25).contains(&total));
        for page in first_total + 2..=total {
            runtime
                .on_interaction(&interaction(Some(true), Some(page), Some(CHANNEL)))
                .await;
        }
        (first_total, total)
    })
    .await;
    let requests = mock.requests();
    mock.shutdown().await;
    drop(runtime);
    let after = state(db.pool(), false).await;
    db.close().await.expect("drop executed-SQL fixture");
    assert_eq!(before, after);
    assert_eq!(requests.len(), total as usize * 2);
    let contents: Vec<_> = requests.chunks_exact(2).map(reply).collect();
    assert_window_pages(&contents, first_total);
    for (pages, range) in [
        (&contents[..first_total as usize], 0..25),
        (&contents[first_total as usize..], 25..50),
    ] {
        assert_eq!(
            pages.iter().flat_map(|text| ids(text)).collect::<Vec<_>>(),
            range.map(|n| format!("row-{n:03}")).collect::<Vec<_>>(),
            "fetch cap applies across packed pages, not to each rendered reply"
        );
    }
    for text in &contents {
        scoped(text);
    }
    let reads: Vec<_> = receipts
        .iter()
        .filter(|query| query.normalized().contains("from scheduled_messages"))
        .collect();
    assert!(
        !reads.is_empty(),
        "no receipt is a broken observer, not proof"
    );
    assert!(
        reads
            .iter()
            .any(|query| query.normalized().contains("count(")),
        "exercise the actual matching count"
    );
    assert!(
        reads
            .iter()
            .any(|query| query.normalized().contains(" offset ")),
        "exercise the actual hidden window"
    );
    for query in reads {
        let sql = query.normalized();
        let predicate = sql
            .split_once(" where ")
            .expect("guild-scoped SQL WHERE")
            .1
            .split(" order by ")
            .next()
            .expect("predicate");
        assert!(
            predicate.contains("guild_id") && predicate.contains("channel_id"),
            "both filters belong in executed SQL, including count/window: {sql}"
        );
        if !sql.contains("count(") {
            assert!(
                sql.contains(" limit "),
                "list SQL must be bounded before decoding: {sql}"
            );
            assert!(
                query.rows > 0 && query.rows <= 25,
                "actual fetched rows obey cap, not just rendered rows: {query:?}"
            );
        }
    }
}

#[tokio::test]
async fn schedule_here_packs_whole_long_rows_with_stable_tied_ids() {
    let db = database().await;
    let long_channel = 1_234_567_890_123_456_000;
    let mut expected = Vec::new();
    for n in (0..25).rev() {
        expected.push(seed(db.pool(), GUILD_S, &long_channel.to_string(), n, true).await);
    }
    expected.reverse();
    for n in 25..29 {
        seed(db.pool(), GUILD_S, OTHER_CHANNEL, n, true).await;
    }
    let before = state(db.pool(), false).await;
    let (texts, total) = packed_pages(db.pool(), Some(true), long_channel, false).await;
    let out_of_range = dispatch(
        db.pool().clone(),
        interaction(Some(true), Some(total + 1), Some(long_channel)),
        true,
    )
    .await;
    let after = state(db.pool(), false).await;
    db.close().await.expect("drop whole-row packing fixture");
    assert_eq!(before, after);
    assert_eq!(texts.len(), total as usize);
    let out_of_range = reply(&out_of_range);
    scoped(&out_of_range);
    assert!(ids(&out_of_range).is_empty());
    assert!(
        out_of_range.contains("out of range") && out_of_range.contains(&format!("{total} page(s)")),
        "range follows actual whole-row packing: {out_of_range}"
    );
    let mut found = Vec::new();
    for (page, text) in texts.iter().enumerate() {
        scoped(text);
        assert_eq!(
            schedule_list_page_indicator(text),
            Some((page as i64 + 1, total))
        );
        for line in schedule_list_row_lines(text) {
            let id = schedule_list_row_id(line);
            let n = expected
                .iter()
                .position(|want| want == id)
                .expect("full copyable configured id");
            assert!(
                line.contains("`<#1234567890123456000>`"),
                "whole channel field: {line}"
            );
            assert!(
                line.contains(&format!("2030-01-01T00:{:02}:00.000Z", n / 2)),
                "whole timestamp: {line}"
            );
            assert_eq!(line.contains(" every 90m"), n.is_multiple_of(3));
            assert_eq!(line.contains(" (disabled)"), n % 5 == 4);
            assert_complete_row(line, id, long_channel, n);
            found.push(id.to_owned());
        }
    }
    assert_eq!(
        found, expected,
        "no lost/duplicated/split/reordered matching id"
    );
    assert!(
        !texts.iter().any(|text| text.contains("truncated")),
        "25 matching rows have no unrelated overflow"
    );
}

// Match the existing live-pool count/window fault fence. The same-named view
// fails only the selected SQL statement; audits can still land independently.
async fn fail_selects(pool: &PgPool, count: bool) {
    sqlx::query("CREATE FUNCTION here_fail_query(p text) RETURNS boolean AS $$
        BEGIN IF current_query() ILIKE p THEN RAISE EXCEPTION 'induced schedule read failure'; END IF;
        RETURN TRUE; END; $$ LANGUAGE plpgsql VOLATILE")
        .execute(pool).await.expect("install read fault");
    sqlx::query("ALTER TABLE scheduled_messages RENAME TO here_schedule_base")
        .execute(pool)
        .await
        .expect("rename under read fault view");
    let ddl = if count {
        "CREATE VIEW scheduled_messages AS SELECT * FROM here_schedule_base WHERE here_fail_query('%count(%')"
    } else {
        "CREATE VIEW scheduled_messages AS SELECT * FROM here_schedule_base WHERE here_fail_query('%OFFSET%')"
    };
    sqlx::query(ddl)
        .execute(pool)
        .await
        .expect("install read fault view");
}

async fn count_outage(here: Option<bool>) {
    let db = database().await;
    for n in (0..27).rev() {
        seed(db.pool(), GUILD_S, CHANNEL_S, n, false).await;
    }
    seed(db.pool(), GUILD_S, OTHER_CHANNEL, 30, false).await;
    let before = state(db.pool(), false).await;
    let (_, first_total) = packed_pages(db.pool(), here, CHANNEL, false).await;
    fail_selects(db.pool(), true).await;
    let (texts, fault_total) = packed_pages(db.pool(), here, CHANNEL, false).await;
    let deep = dispatch(
        db.pool().clone(),
        interaction(here, Some(first_total + 1), Some(CHANNEL)),
        true,
    )
    .await;
    let after = state(db.pool(), true).await;
    db.close().await.expect("drop count-outage fixture");
    assert_eq!(
        before.0, after.0,
        "failed reads preserve schedule/delivery state"
    );
    assert_eq!(fault_total, first_total, "same whole-row first window");
    assert_eq!(
        texts.iter().flat_map(|text| ids(text)).collect::<Vec<_>>(),
        (0..25).map(|n| format!("row-{n:03}")).collect::<Vec<_>>(),
        "25 fetched rows span every packed first-window page despite unknown count"
    );
    for (index, text) in texts.iter().enumerate() {
        if here == Some(true) {
            scoped(text);
        }
        assert_eq!(
            schedule_list_page_indicator(text),
            Some((index as i64 + 1, first_total))
        );
        assert!(
            !text.contains(" more)"),
            "failed count must not invent a numeric remainder: {text}"
        );
        for line in schedule_list_row_lines(text) {
            let id = schedule_list_row_id(line);
            let n = id
                .strip_prefix("row-")
                .expect("seeded id")
                .parse()
                .expect("index");
            assert_complete_row(line, id, CHANNEL, n);
        }
    }
    assert!(
        texts
            .last()
            .expect("terminal first-window page")
            .contains("(truncated)"),
        "unknown count is explicitly incomplete on the terminal packed page"
    );
    assert_eq!(
        reply(&deep),
        SCHEDULE_STORE_FAILURE_REPLY,
        "count outage past all packed first-window pages is not a fabricated out-of-range page"
    );
    assert_eq!(after.1.len(), 1, "preserve existing deep-refusal audit");
}

#[tokio::test]
async fn schedule_here_count_outage_never_claims_completeness_or_false_range() {
    count_outage(Some(true)).await;
}

#[tokio::test]
async fn schedule_here_existing_guild_count_outage_uses_actual_hidden_page() {
    // This corrected paging oracle must pass independently of the new filter.
    for here in [None, Some(false)] {
        count_outage(here).await;
    }
}

#[tokio::test]
async fn schedule_here_hidden_window_outage_fails_closed_without_delivery_writes() {
    for here in [None, Some(false), Some(true)] {
        let db = database().await;
        for n in (0..27).rev() {
            seed(db.pool(), GUILD_S, CHANNEL_S, n, false).await;
        }
        seed(db.pool(), GUILD_S, OTHER_CHANNEL, 30, false).await;
        let before = state(db.pool(), false).await;
        let (_, first_total) = packed_pages(db.pool(), here, CHANNEL, false).await;
        fail_selects(db.pool(), false).await;
        let deep = dispatch(
            db.pool().clone(),
            interaction(here, Some(first_total + 1), Some(CHANNEL)),
            true,
        )
        .await;
        let after = state(db.pool(), true).await;
        db.close().await.expect("drop window-outage fixture");
        assert_eq!(before.0, after.0);
        assert_eq!(
            reply(&deep),
            SCHEDULE_STORE_FAILURE_REPLY,
            "actual OFFSET window outage is not empty or out-of-range"
        );
        assert_eq!(after.1.len(), 1);
        let audit: serde_json::Value = serde_json::from_str(&after.1[0]).expect("audit snapshot");
        assert_eq!(audit["action"], "scheduled.list");
        assert_eq!(audit["outcome"], "rejected");
        assert_eq!(audit["actor_id"], "88");
    }
}

#[tokio::test]
async fn schedule_here_store_observer_rejects_unscoped_and_unbounded_controls() {
    let db = database().await;
    for n in 0..26 {
        seed(db.pool(), GUILD_S, CHANNEL_S, n, false).await;
    }
    seed(db.pool(), GUILD_S, OTHER_CHANNEL, 30, false).await;

    let (scoped, scoped_receipts) = Queries::capture(
        sqlx::query(
            "SELECT id, body FROM scheduled_messages \
             WHERE guild_id = '2222' AND channel_id = '3333' \
             ORDER BY next_run_at, id LIMIT 25",
        )
        .fetch_all(db.pool()),
    )
    .await;
    assert_eq!(
        scoped.expect("bounded matching projection control").len(),
        25
    );
    assert_scoped_schedule_reads(&scoped_receipts);

    let (unbounded, unbounded_receipts) = Queries::capture(
        sqlx::query(
            "SELECT id, body FROM scheduled_messages \
             WHERE guild_id = '2222' AND channel_id = '3333' \
             ORDER BY next_run_at, id",
        )
        .fetch_all(db.pool()),
    )
    .await;
    assert_eq!(
        unbounded
            .expect("unbounded projection control executes")
            .len(),
        26
    );

    let (guild_only, guild_only_receipts) = Queries::capture(
        sqlx::query(
            "SELECT id, body FROM scheduled_messages \
             WHERE guild_id = '2222' ORDER BY next_run_at, id LIMIT 25",
        )
        .fetch_all(db.pool()),
    )
    .await;
    assert_eq!(
        guild_only
            .expect("guild-only projection control executes")
            .len(),
        25
    );
    db.close().await.expect("drop observer controls");

    assert!(
        std::panic::catch_unwind(|| assert_scoped_schedule_reads(&unbounded_receipts)).is_err(),
        "observer must catch all-row fetches"
    );
    assert!(
        std::panic::catch_unwind(|| assert_scoped_schedule_reads(&guild_only_receipts)).is_err(),
        "observer must catch in-memory channel filtering"
    );
}
