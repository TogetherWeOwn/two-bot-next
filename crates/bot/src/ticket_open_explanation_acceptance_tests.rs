#![cfg(test)]

//! Protected behavioral acceptance: Open explains in-flight ticket state and
//! the next eligible open time. Real runtime + store, guarded disposable
//! database, loopback Discord only. The builder must not edit this file.
//!
//! Baseline (pre-implementation) behavior, pinned by
//! `ticket_acceptance_tests::ticket_copy_duplicate_and_cooldown_refusals_are_pinned`:
//! every Existing with a channel says "You already have a ticket: <#...>", every
//! Existing without a channel says "Your ticket is still being created or
//! recovered.", and every Cooldown says "Please wait before opening another
//! ticket." This file requires state-aware refusals and a bounded Discord
//! next-try timestamp instead, with identical reserve decisions and zero
//! refusal effects. RED on baseline, GREEN after the paired implementation.
use super::*;
use crate::{
    command_runtime::CommandRuntime,
    discord_test_common::{MockRest, RestRequest, ScriptedResponse},
};
use serde_json::json;
use two_bot_testsupport::TestDatabase;

async fn database() -> Option<TestDatabase> {
    let Ok(url) = std::env::var("TWO_TEST_DATABASE_URL") else {
        eprintln!("SKIP open-explanation acceptance: TWO_TEST_DATABASE_URL is not set");
        return None;
    };
    Some(
        TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("create migrated disposable test database"),
    )
}

fn runtime(pool: PgPool, mock: &MockRest) -> Arc<TicketRuntime> {
    runtime_with_cooldown(pool, mock, COOLDOWN_SECONDS)
}

fn runtime_with_cooldown(
    pool: PgPool,
    mock: &MockRest,
    cooldown_seconds: u64,
) -> Arc<TicketRuntime> {
    let executor =
        ActionExecutor::with_proxy("ticket-test-token".into(), Some(mock.origin())).unwrap();
    let runtime = Arc::new(
        TicketRuntime::new(
            pool,
            executor,
            TicketConfig {
                guild_id: "100".into(),
                category_id: "200".into(),
                panel_channel_id: "700".into(),
                staff_role_id: "300".into(),
                cooldown_seconds,
            },
            HashMap::new(),
        )
        .unwrap(),
    );
    runtime.set_bot_id(400);
    runtime
}

fn button(action: TicketAction, guild: &str, actor: &str, username: &str, id: &str) -> Interaction {
    let custom_id = match action {
        TicketAction::Open => TICKET_OPEN_ID,
        TicketAction::Claim => TICKET_CLAIM_ID,
        TicketAction::Close => TICKET_CLOSE_ID,
    };
    serde_json::from_value(json!({
        "application_id":"400", "authorizing_integration_owners":{"0":guild},
        "id":id, "token":format!("ticket-test-{id}"), "type":3, "version":1,
        "guild_id":guild, "channel":{"id":"600","type":0,"name":"ticket-member","permissions":"0"},
        "data":{"custom_id":custom_id,"component_type":2},
        "member":{
            "roles":[], "permissions":"0", "deaf":false,"mute":false,"flags":0,
            "user":{"id":actor,"username":username,"discriminator":"0","avatar":null}
        }
    }))
    .expect("synthetic wire-format ticket button")
}

fn open_button(actor: &str) -> Interaction {
    button(TicketAction::Open, "100", actor, "Member Name", "800")
}

fn category_doc() -> Value {
    json!({"id":"200","guild_id":"100","type":4})
}

/// Refusal-path double: the category fetch succeeds, everything else is a
/// loud 500 so any extra Discord work (channel create/post/delete) is both
/// recorded and would fail the runtime if attempted.
async fn refusal_mock() -> MockRest {
    MockRest::with_responder(|request| {
        if request.method == "GET" && request.path.ends_with("/channels/200") {
            ScriptedResponse::json(200, category_doc())
        } else {
            ScriptedResponse::status(500)
        }
    })
    .await
}

/// Router-path double: defer + category + edit succeed, everything else 500.
async fn router_mock() -> MockRest {
    MockRest::with_responder(|request| {
        if request.path.ends_with("/callback") {
            ScriptedResponse::status(204)
        } else if request.method == "GET" && request.path.ends_with("/channels/200") {
            ScriptedResponse::json(200, category_doc())
        } else if request.method == "PATCH" {
            ScriptedResponse::json(200, json!({"id":"900"}))
        } else {
            ScriptedResponse::status(500)
        }
    })
    .await
}

fn member_reply(result: Result<String>) -> String {
    match result {
        Ok(reply) => reply,
        Err(error) => error.reply(),
    }
}

async fn ticket_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM tickets")
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn transcript_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM ticket_transcripts")
        .fetch_one(pool)
        .await
        .unwrap()
}

fn assert_only_category_get(requests: &[RestRequest]) {
    assert_eq!(
        requests.len(),
        1,
        "a refusal fetches the category once and does nothing else: {requests:?}"
    );
    assert_eq!(requests[0].method, "GET");
    assert!(
        requests[0].path.ends_with("/channels/200"),
        "unexpected refusal request: {:?}",
        requests[0]
    );
}

fn assert_no_mutating_discord(requests: &[RestRequest]) {
    for request in requests {
        assert!(
            request.method == "GET"
                || request.path.ends_with("/callback")
                || request.method == "PATCH",
            "refusal must not create/post/delete Discord state: {:?}",
            request
        );
        assert!(
            !request.path.ends_with("/channels/600/messages")
                && !request.path.contains("/guilds/100/channels"),
            "no channel or control work on refusal: {request:?}"
        );
    }
    assert!(
        !requests
            .iter()
            .any(|r| r.method == "POST" && !r.path.ends_with("/callback")),
        "no POST work on refusal: {requests:?}"
    );
    assert!(
        !requests.iter().any(|r| r.method == "DELETE"),
        "no DELETE work on refusal: {requests:?}"
    );
    assert!(
        !requests.iter().any(|r| r.method == "PUT"),
        "no permission work on refusal: {requests:?}"
    );
}

/// Parse the first `<t:SECONDS:STYLE>` in a reply. Returns (seconds, style).
fn discord_timestamp(reply: &str) -> Option<(u64, char)> {
    let start = reply.find("<t:")? + 3;
    let rest = &reply[start..];
    let colon = rest.find(':')?;
    let end = rest.find('>')?;
    if colon > end {
        return None;
    }
    let seconds: u64 = rest[..colon].parse().ok()?;
    let style = rest[colon + 1..end].chars().next()?;
    if !"tTdDfFR".contains(style) {
        return None;
    }
    Some((seconds, style))
}

async fn setup_creating_with_channel(
    runtime: &TicketRuntime,
    id: &str,
    opener: &str,
    channel: &str,
) {
    assert!(matches!(
        runtime.store.reserve(id, opener, 0, 0).await.unwrap(),
        OpenResult::Created(_)
    ));
    runtime.store.record_channel(id, channel).await.unwrap();
}

async fn setup_open(runtime: &TicketRuntime, id: &str, opener: &str, channel: &str) {
    assert!(matches!(
        runtime.store.reserve(id, opener, 0, 0).await.unwrap(),
        OpenResult::Created(_)
    ));
    runtime.store.record_channel(id, channel).await.unwrap();
    runtime.store.activate(id, channel).await.unwrap();
}

async fn setup_closing(runtime: &TicketRuntime, id: &str, opener: &str, channel: &str, now: i64) {
    setup_open(runtime, id, opener, channel).await;
    runtime.store.begin_close(id, now).await.unwrap();
}

async fn setup_cleanup(runtime: &TicketRuntime, id: &str, opener: &str, channel: &str, now: i64) {
    setup_open(runtime, id, opener, channel).await;
    let closing = runtime.store.begin_close(id, now).await.unwrap();
    runtime
        .store
        .save_transcript(
            id,
            closing.closing_started_at.unwrap(),
            now + 1,
            format_transcript(vec![]),
        )
        .await
        .unwrap();
}

/// Close a ticket whose creation time is explicit (for deterministic
/// next-eligible math), rather than the real clock. The close itself uses
/// close_at for its lifecycle stamps; created_at stays `created`.
async fn setup_closed_at(
    runtime: &TicketRuntime,
    pool: &PgPool,
    id: &str,
    opener: &str,
    channel: &str,
    created: i64,
    close_at: i64,
) {
    use two_bot_core::funnel::format_iso_millis;
    assert!(matches!(
        runtime.store.reserve(id, opener, created, 0).await.unwrap(),
        OpenResult::Created(_)
    ));
    runtime.store.record_channel(id, channel).await.unwrap();
    runtime.store.activate(id, channel).await.unwrap();
    let closing = runtime.store.begin_close(id, close_at).await.unwrap();
    runtime
        .store
        .save_transcript(
            id,
            closing.closing_started_at.unwrap(),
            close_at + 1,
            format_transcript(vec![]),
        )
        .await
        .unwrap();
    runtime
        .store
        .finish_cleanup(id, close_at + 2)
        .await
        .unwrap();
    // Belt and braces: the reserve above already stored `created`, but an
    // explicit rewrite keeps the math obvious if the store ever normalizes.
    sqlx::query("UPDATE tickets SET created_at = $1 WHERE id = $2")
        .bind(format_iso_millis(created))
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
}

async fn wait_for_reply(mock: &MockRest, id: &str) -> Value {
    let suffix = format!("/ticket-test-{id}/messages/@original");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(request) = mock
                .requests()
                .into_iter()
                .find(|request| request.method == "PATCH" && request.path.ends_with(&suffix))
            {
                return serde_json::from_slice(&request.body).unwrap();
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("shared router must finish its deferred reply")
}

fn assert_mentions_suppressed(body: &Value) {
    assert_eq!(body["allowed_mentions"]["parse"], json!([]));
    for key in ["users", "roles"] {
        assert!(
            body["allowed_mentions"][key].is_null() || body["allowed_mentions"][key] == json!([]),
            "no explicit ping allowlist in {body}"
        );
    }
    assert_ne!(body["allowed_mentions"]["replied_user"], json!(true));
}

// Distinct private replies per lifecycle state; only Open is usable. RED on
// baseline: Creating/Closing/CleanupPending with channels all say "You already
// have a ticket: <#...>" exactly like Open.
#[tokio::test]
async fn open_states_are_distinct_and_only_open_is_usable() {
    let Some(db) = database().await else { return };
    let mock = refusal_mock().await;
    // Four openers, four channels: one active ticket per (guild, opener).
    let tickets = runtime(db.pool().clone(), &mock);
    setup_creating_with_channel(&tickets, "ex-creating", "500", "610").await;
    setup_open(&tickets, "ex-open", "501", "611").await;
    setup_closing(&tickets, "ex-closing", "502", "612", 10).await;
    setup_cleanup(&tickets, "ex-cleanup", "503", "613", 10).await;
    let before_tickets = ticket_count(db.pool()).await;
    let before_transcripts = transcript_count(db.pool()).await;

    let creating = member_reply(
        tickets
            .execute(&open_button("500"), TicketAction::Open)
            .await,
    );
    let open = member_reply(
        tickets
            .execute(&open_button("501"), TicketAction::Open)
            .await,
    );
    let closing = member_reply(
        tickets
            .execute(&open_button("502"), TicketAction::Open)
            .await,
    );
    let cleanup = member_reply(
        tickets
            .execute(&open_button("503"), TicketAction::Open)
            .await,
    );

    let requests = mock.requests();
    let after_tickets = ticket_count(db.pool()).await;
    let after_transcripts = transcript_count(db.pool()).await;
    db.close().await.unwrap();
    mock.shutdown().await;

    // No-effect: explaining state creates nothing and captures nothing.
    assert_eq!(after_tickets, before_tickets);
    assert_eq!(after_transcripts, before_transcripts);
    assert_eq!(
        after_transcripts, 1,
        "only the cleanup fixture has a transcript"
    );
    assert_eq!(requests.len(), 4, "one category fetch per refused open");
    assert_no_mutating_discord(&requests);

    // Open with a channel is the only usable ticket.
    assert!(
        open.contains("<#611>"),
        "open names its usable channel: {open}"
    );
    let open_lower = open.to_lowercase();
    assert!(
        open_lower.contains("already") || open_lower.contains("open"),
        "open says the ticket exists: {open}"
    );
    assert!(
        !open_lower.contains("closing")
            && !open_lower.contains("cleanup")
            && !open_lower.contains("cleaning")
            && !open_lower.contains("being created"),
        "open is not confused with in-flight states: {open}"
    );

    // Creating with a channel is recovery-pending, never a ready ticket.
    let creating_lower = creating.to_lowercase();
    assert!(
        creating_lower.contains("creat") || creating_lower.contains("recover"),
        "creating explains recovery, not readiness: {creating}"
    );
    assert!(
        !creating.contains("<#610>"),
        "creating must not present its channel as a ready ticket: {creating}"
    );
    assert!(
        !creating_lower.contains("already have a ticket"),
        "creating is distinct from the usable-ticket reply: {creating}"
    );

    // Closing is capture-in-progress, never a ready ticket.
    let closing_lower = closing.to_lowercase();
    assert!(
        closing_lower.contains("clos"),
        "closing names its lifecycle state: {closing}"
    );
    assert!(
        !closing.contains("<#612>"),
        "closing must not present its channel as a ready ticket: {closing}"
    );
    assert!(
        !closing_lower.contains("already have a ticket"),
        "closing is distinct from the usable-ticket reply: {closing}"
    );

    // Cleanup-pending is deletion-in-progress, never a ready ticket.
    let cleanup_lower = cleanup.to_lowercase();
    assert!(
        cleanup_lower.contains("clean")
            || cleanup_lower.contains("clos")
            || cleanup_lower.contains("delet"),
        "cleanup names its lifecycle state: {cleanup}"
    );
    assert!(
        !cleanup.contains("<#613>"),
        "cleanup must not present its channel as a ready ticket: {cleanup}"
    );
    assert!(
        !cleanup_lower.contains("already have a ticket"),
        "cleanup is distinct from the usable-ticket reply: {cleanup}"
    );

    // All four replies are distinct.
    let mut distinct = vec![
        creating.as_str(),
        open.as_str(),
        closing.as_str(),
        cleanup.as_str(),
    ];
    distinct.sort_unstable();
    distinct.dedup();
    assert_eq!(distinct.len(), 4, "each state has its own reply");

    // Concise guidance, not a wall of text.
    for reply in [&creating, &open, &closing, &cleanup] {
        assert!(reply.len() < 1000, "concise refusal: {reply}");
    }
}

// Creating without a channel keeps its recovery guidance. CONTROL: passes on
// baseline and must keep passing (no channel link, recovery wording).
#[tokio::test]
async fn creating_without_channel_stays_recovery_guidance() {
    let Some(db) = database().await else { return };
    let mock = refusal_mock().await;
    let tickets = runtime(db.pool().clone(), &mock);
    assert!(matches!(
        tickets
            .store
            .reserve("ex-creating-bare", "500", 0, 0)
            .await
            .unwrap(),
        OpenResult::Created(_)
    ));
    let before = ticket_count(db.pool()).await;
    let reply = member_reply(
        tickets
            .execute(&open_button("500"), TicketAction::Open)
            .await,
    );
    let requests = mock.requests();
    let after = ticket_count(db.pool()).await;
    db.close().await.unwrap();
    mock.shutdown().await;

    assert_eq!(after, before);
    assert_only_category_get(&requests);
    let lower = reply.to_lowercase();
    assert!(
        lower.contains("creat") || lower.contains("recover"),
        "bare creating explains recovery: {reply}"
    );
    assert!(!reply.contains("<#"), "no channel to name: {reply}");
}

// Closing/CleanupPending without channel identity still name their lifecycle
// state, not creation. RED on baseline: channel-less refusals always say
// "still being created or recovered." Rows are inserted via SQL because the
// domain transitions require a channel; legacy/out-of-band rows must still
// explain honestly.
#[tokio::test]
async fn closing_and_cleanup_without_channel_name_lifecycle_not_creating() {
    let Some(db) = database().await else { return };
    use two_bot_core::funnel::format_iso_millis;
    let mock = refusal_mock().await;
    let tickets = runtime(db.pool().clone(), &mock);
    let now = now_millis_for_test();
    for (id, opener, status) in [
        ("ex-closing-bare", "500", "closing"),
        ("ex-cleanup-bare", "501", "cleanup_pending"),
    ] {
        sqlx::query(
            "INSERT INTO tickets (id, guild_id, opener_id, status, created_at) VALUES ($1, '100', $2, $3, $4)",
        )
        .bind(id)
        .bind(opener)
        .bind(status)
        .bind(format_iso_millis(now))
        .execute(db.pool())
        .await
        .unwrap();
    }
    let closing = member_reply(
        tickets
            .execute(&open_button("500"), TicketAction::Open)
            .await,
    );
    let cleanup = member_reply(
        tickets
            .execute(&open_button("501"), TicketAction::Open)
            .await,
    );
    let requests = mock.requests();
    db.close().await.unwrap();
    mock.shutdown().await;

    assert_eq!(requests.len(), 2);
    assert_no_mutating_discord(&requests);
    let closing_lower = closing.to_lowercase();
    assert!(
        closing_lower.contains("clos"),
        "channel-less closing names closing: {closing}"
    );
    assert!(
        !closing_lower.contains("being created"),
        "closing is not creation: {closing}"
    );
    assert!(!closing.contains("<#"), "no channel to name: {closing}");
    let cleanup_lower = cleanup.to_lowercase();
    assert!(
        cleanup_lower.contains("clean")
            || cleanup_lower.contains("clos")
            || cleanup_lower.contains("delet"),
        "channel-less cleanup names cleanup: {cleanup}"
    );
    assert!(
        !cleanup_lower.contains("being created"),
        "cleanup is not creation: {cleanup}"
    );
    assert!(!cleanup.contains("<#"), "no channel to name: {cleanup}");
}

// Cooldown names a bounded Discord next-try time, worded as the next time to
// try rather than a reservation or guarantee. RED on baseline: the reply is a
// bare "Please wait before opening another ticket." with no timestamp.
#[tokio::test]
async fn cooldown_names_bounded_next_try_time() {
    let Some(db) = database().await else { return };
    let mock = refusal_mock().await;
    let tickets = runtime(db.pool().clone(), &mock);
    let created = now_millis_for_test();
    setup_closed_at(
        &tickets,
        db.pool(),
        "ex-cooldown",
        "500",
        "620",
        created,
        created + 1,
    )
    .await;
    let before_tickets = ticket_count(db.pool()).await;
    let before_transcripts = transcript_count(db.pool()).await;

    let reply = member_reply(
        tickets
            .execute(&open_button("500"), TicketAction::Open)
            .await,
    );

    let requests = mock.requests();
    let after_tickets = ticket_count(db.pool()).await;
    let after_transcripts = transcript_count(db.pool()).await;
    db.close().await.unwrap();
    mock.shutdown().await;

    assert_eq!(after_tickets, before_tickets, "cooldown creates no row");
    assert_eq!(
        after_transcripts, before_transcripts,
        "cooldown captures nothing"
    );
    assert_only_category_get(&requests);

    let lower = reply.to_lowercase();
    assert!(
        lower.contains("try") || lower.contains("retry"),
        "cooldown says when to try next: {reply}"
    );
    assert!(
        lower.contains("next"),
        "cooldown words it as the next time to try: {reply}"
    );
    assert!(
        !lower.contains("reserv") && !lower.contains("guarantee"),
        "a next-try hint is not a reservation or guarantee: {reply}"
    );
    let (seconds, _) = discord_timestamp(&reply)
        .unwrap_or_else(|| panic!("cooldown carries a Discord timestamp: {reply}"));
    let expected = ((created + 300_000) / 1000) as u64;
    assert_eq!(
        seconds, expected,
        "next-eligible is last creation + cooldown: {reply}"
    );
    let now_seconds = (now_millis_for_test() / 1000).max(0) as u64;
    assert!(
        seconds > now_seconds,
        "next-eligible is in the future, not already-eligible: {reply}"
    );
    assert!(reply.len() < 1000, "concise refusal: {reply}");
}

// Future stored timestamps and maximum configured cooldown stay
// overflow-safe: no panic, no wraparound to the past, no misleading
// already-eligible timestamp. RED on baseline (no timestamp at all).
#[tokio::test]
async fn cooldown_future_and_max_inputs_stay_safe() {
    let Some(db) = database().await else { return };

    // Future stored creation: last is ahead of the real clock.
    let mock = refusal_mock().await;
    let tickets = runtime(db.pool().clone(), &mock);
    let now = now_millis_for_test();
    let future_created = now + 60_000;
    setup_closed_at(
        &tickets,
        db.pool(),
        "ex-cooldown-future",
        "500",
        "621",
        future_created,
        future_created + 1,
    )
    .await;
    let future_reply = member_reply(
        tickets
            .execute(&open_button("500"), TicketAction::Open)
            .await,
    );
    let future_requests = mock.requests();
    mock.shutdown().await;

    assert_only_category_get(&future_requests);
    let (future_seconds, _) = discord_timestamp(&future_reply)
        .unwrap_or_else(|| panic!("future cooldown carries a timestamp: {future_reply}"));
    let expected_future = ((future_created + 300_000) / 1000) as u64;
    assert_eq!(
        future_seconds, expected_future,
        "future last + cooldown, no clamp to now: {future_reply}"
    );
    assert!(
        future_seconds > (now / 1000).max(0) as u64,
        "future next-eligible stays in the future: {future_reply}"
    );

    // Maximum configured cooldown: saturating math, still a future timestamp.
    let mock = refusal_mock().await;
    let max_tickets = runtime_with_cooldown(db.pool().clone(), &mock, u64::MAX);
    let max_created = now_millis_for_test();
    setup_closed_at(
        &max_tickets,
        db.pool(),
        "ex-cooldown-max",
        "501",
        "622",
        max_created,
        max_created + 1,
    )
    .await;
    let max_reply = member_reply(
        max_tickets
            .execute(&open_button("501"), TicketAction::Open)
            .await,
    );
    let max_requests = mock.requests();
    db.close().await.unwrap();
    mock.shutdown().await;

    assert_only_category_get(&max_requests);
    let (max_seconds, _) = discord_timestamp(&max_reply)
        .unwrap_or_else(|| panic!("max cooldown carries a timestamp: {max_reply}"));
    let now_seconds = (now_millis_for_test() / 1000).max(0) as u64;
    assert!(
        max_seconds > now_seconds,
        "max cooldown is future, never wrapped to the past: {max_reply}"
    );
    assert!(
        !max_reply.contains("1970"),
        "no epoch wraparound: {max_reply}"
    );
    let lower = max_reply.to_lowercase();
    assert!(
        !lower.contains("reserv") && !lower.contains("guarantee"),
        "max cooldown is still a hint, not a reservation: {max_reply}"
    );
}

// Reserve decisions are unchanged: cooldown boundary is exact and an active
// ticket keeps precedence over cooldown. CONTROL via store decisions only
// (no inner-shape pin, so the enriched projection stays compatible).
#[tokio::test]
async fn cooldown_boundary_and_active_precedence_are_preserved() {
    let Some(db) = database().await else { return };
    let mock = refusal_mock().await;
    let tickets = runtime(db.pool().clone(), &mock);

    // Exact boundary: just-before refuses, exactly-at and after reserve.
    assert!(matches!(
        tickets
            .store
            .reserve("bd-1", "510", 0, COOLDOWN_SECONDS)
            .await
            .unwrap(),
        OpenResult::Created(_)
    ));
    // Close it so only the cooldown (last_created_at = 0) remains.
    tickets.store.record_channel("bd-1", "630").await.unwrap();
    tickets.store.activate("bd-1", "630").await.unwrap();
    let closing = tickets.store.begin_close("bd-1", 1).await.unwrap();
    tickets
        .store
        .save_transcript(
            "bd-1",
            closing.closing_started_at.unwrap(),
            2,
            format_transcript(vec![]),
        )
        .await
        .unwrap();
    tickets.store.finish_cleanup("bd-1", 3).await.unwrap();
    assert!(matches!(
        tickets
            .store
            .reserve("bd-2", "510", 299_999, COOLDOWN_SECONDS)
            .await
            .unwrap(),
        OpenResult::Refused(_)
    ));
    assert!(matches!(
        tickets
            .store
            .reserve("bd-3", "510", 300_000, COOLDOWN_SECONDS)
            .await
            .unwrap(),
        OpenResult::Created(_)
    ));

    // Active precedence: with an active ticket, reserve refuses even where a
    // bare cooldown would also refuse; the refusal is the active-ticket one.
    // Opener 511 holds an active Creating reservation from t=0.
    assert!(matches!(
        tickets
            .store
            .reserve("ap-1", "511", 0, COOLDOWN_SECONDS)
            .await
            .unwrap(),
        OpenResult::Created(_)
    ));
    let active_refusal = tickets
        .store
        .reserve("ap-2", "511", 1, COOLDOWN_SECONDS)
        .await
        .unwrap();
    assert!(
        matches!(active_refusal, OpenResult::Refused(_)),
        "active ticket refuses"
    );
    // Same timestamps without an active ticket would be a cooldown refusal;
    // both refuse, but the runtime renders them differently (state vs time).
    // That rendering split is pinned by the state and cooldown tests above.
    // Rows: bd-1 (closed), bd-3 (active at boundary), ap-1 (active).
    assert_eq!(ticket_count(db.pool()).await, 3);
    assert!(mock.requests().is_empty(), "store-only, no Discord");
    db.close().await.unwrap();
    mock.shutdown().await;
}

// Per-(guild, opener) isolation: the same opener in another guild and another
// member in the same guild are unaffected, and neither learns the other's
// channel. CONTROL.
#[tokio::test]
async fn same_opener_in_other_guild_and_other_member_are_isolated() {
    let Some(db) = database().await else { return };
    let mock = refusal_mock().await;
    let tickets = runtime(db.pool().clone(), &mock);
    setup_open(&tickets, "iso-mine", "500", "640").await;

    // Same opener, other guild: a fresh reserve, not a refusal.
    let other_guild = TicketStore::new(db.pool().clone(), "101".into()).unwrap();
    let cross = other_guild.reserve("iso-cross", "500", 0, 0).await.unwrap();
    assert!(
        matches!(cross, OpenResult::Created(_)),
        "guild isolation: same opener may open elsewhere"
    );

    // Other member, same guild: also a fresh reserve.
    let peer = tickets
        .store
        .reserve("iso-peer", "501", 0, 0)
        .await
        .unwrap();
    assert!(
        matches!(peer, OpenResult::Created(_)),
        "opener isolation: another member may open"
    );

    // Neither new ticket discloses the existing channel.
    let stored: Vec<(String, Option<String>)> =
        sqlx::query_as("SELECT id, channel_id FROM tickets WHERE id IN ('iso-cross', 'iso-peer')")
            .fetch_all(db.pool())
            .await
            .unwrap();
    for (id, channel) in stored {
        assert_ne!(
            channel.as_deref(),
            Some("640"),
            "no cross-ticket leak into {id}"
        );
    }
    assert!(mock.requests().is_empty(), "store-only, no Discord");
    db.close().await.unwrap();
    mock.shutdown().await;
}

// Concurrent double Open keeps a single reservation: one Created, one
// Refused, one row. CONTROL: the per-(guild, opener) lock still serializes
// inside the reservation transaction (no extra preflight decides eligibility).
#[tokio::test]
async fn concurrent_double_open_keeps_single_row() {
    let Some(db) = database().await else { return };
    let mock = refusal_mock().await;
    let tickets = runtime(db.pool().clone(), &mock);
    let (first, second) = tokio::join!(
        tickets.store.reserve("race-1", "500", 0, 0),
        tickets.store.reserve("race-2", "500", 0, 0)
    );
    let first = first.unwrap();
    let second = second.unwrap();
    let created = [&first, &second]
        .iter()
        .filter(|r| matches!(r, OpenResult::Created(_)))
        .count();
    let refused = [&first, &second]
        .iter()
        .filter(|r| matches!(r, OpenResult::Refused(_)))
        .count();
    assert_eq!((created, refused), (1, 1), "exactly one reservation wins");
    assert_eq!(ticket_count(db.pool()).await, 1);
    assert!(mock.requests().is_empty(), "store-only, no Discord");
    db.close().await.unwrap();
    mock.shutdown().await;
}

// Invalid stored timestamps and sick stores stay honest: a generic failure,
// never cooldown copy or a fabricated timestamp. CONTROL.
#[tokio::test]
async fn invalid_timestamp_and_store_failure_stay_honest() {
    let Some(db) = database().await else { return };

    // Invalid created_at breaks the reservation's timestamptz ordering, so the
    // store errors and the runtime must not call it a cooldown.
    sqlx::query(
        "INSERT INTO tickets (id, guild_id, opener_id, status, created_at) VALUES ('bad-ts', '100', '500', 'closed', 'not-a-timestamp')",
    )
    .execute(db.pool())
    .await
    .unwrap();
    let mock = refusal_mock().await;
    let tickets = runtime(db.pool().clone(), &mock);
    let bad_result = tickets
        .execute(&open_button("500"), TicketAction::Open)
        .await;
    let bad_is_storage = matches!(
        &bad_result,
        Err(Failure::Store(StoreError::Database(_)))
            | Err(Failure::Store(StoreError::InvalidRow(_)))
    );
    let bad_reply = member_reply(bad_result);
    let bad_requests = mock.requests();
    mock.shutdown().await;
    assert!(
        bad_is_storage,
        "invalid row is a store failure: {bad_reply}"
    );
    assert!(
        discord_timestamp(&bad_reply).is_none(),
        "no fabricated timestamp on store failure: {bad_reply}"
    );
    assert!(
        !bad_reply.to_lowercase().contains("next"),
        "store failure is not a next-try hint: {bad_reply}"
    );
    assert_eq!(bad_requests.len(), 1, "category fetch precedes the failure");

    // Sick store (tables dropped): same honest generic failure, no cooldown.
    sqlx::query("DROP TABLE ticket_transcripts")
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query("DROP TABLE tickets")
        .execute(db.pool())
        .await
        .unwrap();
    let mock = refusal_mock().await;
    let sick = runtime(db.pool().clone(), &mock);
    let sick_result = sick.execute(&open_button("501"), TicketAction::Open).await;
    let sick_reply = member_reply(sick_result);
    let sick_requests = mock.requests();
    // The disposable database is dropped by close even without its tables.
    db.close().await.unwrap();
    mock.shutdown().await;

    assert!(
        sick_reply.to_lowercase().contains("failed")
            || sick_reply.to_lowercase().contains("try again"),
        "sick store is an honest failure: {sick_reply}"
    );
    assert!(
        discord_timestamp(&sick_reply).is_none(),
        "no timestamp on store failure: {sick_reply}"
    );
    assert_eq!(sick_requests.len(), 1);
}

// Actual button path: a hostile display name pressing Open on a Closing
// ticket gets a private state-aware refusal with suppressed mentions and no
// Discord work beyond defer + category + edit. RED on the copy (baseline says
// "already have a ticket"), CONTROL on the ephemeral/suppression/no-work
// shape.
#[tokio::test]
async fn router_refusal_is_private_suppressed_and_state_aware_with_hostile_name() {
    let Some(db) = database().await else { return };
    let mock = router_mock().await;
    let tickets = runtime(db.pool().clone(), &mock);
    setup_closing(&tickets, "router-closing", "500", "650", 10).await;
    let commands = CommandRuntime::with_tickets(
        db.pool().clone(),
        tickets.executor.clone(),
        Arc::clone(&tickets),
    );
    let hostile = button(
        TicketAction::Open,
        "100",
        "500",
        "@everyone <@&300> <@501> hostile",
        "810",
    );
    commands.on_interaction(&hostile).await;
    let reply = wait_for_reply(&mock, "810").await;
    let requests = mock.requests();
    let tickets_after = ticket_count(db.pool()).await;
    let transcripts_after = transcript_count(db.pool()).await;
    db.close().await.unwrap();
    mock.shutdown().await;

    assert_eq!(tickets_after, 1, "refusal creates no row");
    assert_eq!(transcripts_after, 0, "refusal captures no transcript");
    let content = reply["content"].as_str().expect("router reply content");
    let lower = content.to_lowercase();
    assert!(
        lower.contains("clos"),
        "router refusal names the closing state: {content}"
    );
    assert!(
        !content.contains("<#650>"),
        "closing channel is not presented as ready: {content}"
    );
    assert!(
        !content.contains("@everyone") && !content.contains("<@&") && !content.contains("<@501>"),
        "hostile name never leaks into the reply: {content}"
    );
    assert_mentions_suppressed(&reply);

    let defers: Vec<Value> = requests
        .iter()
        .filter(|r| r.path.ends_with("/callback"))
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    assert_eq!(defers.len(), 1, "one defer");
    assert_eq!(defers[0]["type"], 5);
    assert_eq!(defers[0]["data"]["flags"], 64, "defer is ephemeral");
    assert_no_mutating_discord(&requests);
    assert!(
        requests
            .iter()
            .any(|r| r.method == "GET" && r.path.ends_with("/channels/200")),
        "refusal still validates the category: {requests:?}"
    );
}

// Cooldown never discloses transcript bodies or another member's channel.
// CONTROL: the bare baseline reply already avoids both; the enriched reply
// must keep that property.
#[tokio::test]
async fn cooldown_does_not_disclose_transcript_or_other_member_channel() {
    let Some(db) = database().await else { return };
    const SENTINEL: &str = "SENTINEL_PRIVATE_SUPPORT_DETAIL_MUST_NOT_LEAK";
    let mock = refusal_mock().await;
    let tickets = runtime(db.pool().clone(), &mock);
    // Other member's usable ticket.
    setup_open(&tickets, "leak-other", "501", "660").await;
    // My closed ticket with a transcript body carrying the sentinel.
    assert!(matches!(
        tickets
            .store
            .reserve("leak-mine", "500", now_millis_for_test(), 0)
            .await
            .unwrap(),
        OpenResult::Created(_)
    ));
    tickets
        .store
        .record_channel("leak-mine", "661")
        .await
        .unwrap();
    tickets.store.activate("leak-mine", "661").await.unwrap();
    let closing = tickets
        .store
        .begin_close("leak-mine", now_millis_for_test() + 1)
        .await
        .unwrap();
    let snapshot = TranscriptSnapshot {
        content: format!("[1970-01-01T00:00:00.000Z] member: {SENTINEL}"),
        message_count: 1,
    };
    tickets
        .store
        .save_transcript(
            "leak-mine",
            closing.closing_started_at.unwrap(),
            now_millis_for_test() + 2,
            snapshot,
        )
        .await
        .unwrap();
    tickets
        .store
        .finish_cleanup("leak-mine", now_millis_for_test() + 3)
        .await
        .unwrap();

    let reply = member_reply(
        tickets
            .execute(&open_button("500"), TicketAction::Open)
            .await,
    );
    db.close().await.unwrap();
    mock.shutdown().await;

    assert!(
        !reply.contains(SENTINEL),
        "cooldown never leaks transcript bodies: {reply}"
    );
    assert!(
        !reply.contains("<#660>") && !reply.contains("660"),
        "cooldown never names another member's channel: {reply}"
    );
    assert!(
        !reply.contains("<#661>") && !reply.contains("661"),
        "cooldown never names the closed channel as usable: {reply}"
    );
}

// Wrong-guild Open still refuses before any storage or Discord work.
// CONTROL.
#[tokio::test]
async fn wrong_guild_still_refuses_before_storage_or_discord() {
    let Some(db) = database().await else { return };
    let mock = refusal_mock().await;
    let tickets = runtime(db.pool().clone(), &mock);
    let foreign = button(TicketAction::Open, "999", "500", "Member Name", "811");
    let result = tickets.execute(&foreign, TicketAction::Open).await;
    assert!(
        matches!(
            result,
            Err(Failure::Store(StoreError::Domain(TicketError::WrongGuild)))
        ),
        "wrong guild is a domain refusal"
    );
    assert_eq!(ticket_count(db.pool()).await, 0);
    assert!(mock.requests().is_empty(), "refused before any Discord");
    db.close().await.unwrap();
    mock.shutdown().await;
}
