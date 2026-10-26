//! Protected outcome tests: list read snapshots and bounded removal promises.
//! The builder changes product code, not this file. The manifest is the test
//! matrix/recipe; no live Discord, scheduler model, sleeps or new runtime APIs.

#![cfg(test)]

use super::*;
use serde_json::{json, Value};
use two_bot_core::{get_scheduled, ScheduledMessageRow};
use two_bot_testsupport::TestDatabase;

use crate::discord_test_common::{MockRest as GatedRest, ScriptedResponse};

const AT: &str = "2030-01-01T00:00:00.000Z";
const CLAIM: &str = "private-claim-capability-sentinel";
const NONCE: &str = "private-occurrence-nonce-sentinel";
const BODY: &str = "private-body-sentinel @everyone <@&9999>";
const T0: u64 = 1_790_000_000_000;

#[derive(Clone, Copy)]
enum View {
    Ready,
    Disabled,
    Claimed,
    Fenced,
}

struct Case {
    name: &'static str,
    enabled: bool,
    claimed: bool,
    fence: Option<bool>,
    view: View,
}

// Literal expectations, independent of any production state-classifier.
const CASES: &[Case] = &[
    Case {
        name: "ready",
        enabled: true,
        claimed: false,
        fence: Some(false),
        view: View::Ready,
    },
    Case {
        name: "claimed",
        enabled: true,
        claimed: true,
        fence: Some(false),
        view: View::Claimed,
    },
    Case {
        name: "fenced",
        enabled: true,
        claimed: false,
        fence: Some(true),
        view: View::Fenced,
    },
    Case {
        name: "both",
        enabled: true,
        claimed: true,
        fence: Some(true),
        view: View::Fenced,
    },
    Case {
        name: "disabled",
        enabled: false,
        claimed: false,
        fence: Some(false),
        view: View::Disabled,
    },
    Case {
        name: "disabled-claim",
        enabled: false,
        claimed: true,
        fence: Some(false),
        view: View::Claimed,
    },
    Case {
        name: "disabled-fence",
        enabled: false,
        claimed: false,
        fence: Some(true),
        view: View::Fenced,
    },
    Case {
        name: "disabled-both",
        enabled: false,
        claimed: true,
        fence: Some(true),
        view: View::Fenced,
    },
    // The existing store treats a restored NULL fence conservatively.
    Case {
        name: "null-fence",
        enabled: true,
        claimed: false,
        fence: None,
        view: View::Fenced,
    },
];

async fn fixture() -> TestDatabase {
    let url = std::env::var("TWO_TEST_DATABASE_URL")
        .expect("acceptance requires the guarded disposable test database; absent env is not PASS");
    TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("create migrated disposable fixture")
}

async fn seed(pool: &PgPool, id: &str, case: &Case, channel: &str, at: &str) {
    sqlx::query(
        "INSERT INTO scheduled_messages
         (id, guild_id, channel_id, body, next_run_at, interval_seconds, enabled,
          created_by, created_at, updated_by, updated_at, claim_token, claimed_at,
          occurrence_nonce, delivery_uncertain)
         VALUES ($1, $2, $3, $4, $5, 3600, $6, '77', '2020-01-01T00:00:00.000Z',
                 '77', '2020-01-01T00:00:00.000Z', $7, $8, $9, $10)",
    )
    .bind(id)
    .bind(GUILD_S)
    .bind(channel)
    .bind(BODY)
    .bind(at)
    .bind(case.enabled)
    .bind(case.claimed.then_some(CLAIM))
    // Deliberately stale: a claim is evidence of a lease, not a live worker.
    .bind(case.claimed.then_some("2020-01-01T00:00:00.000Z"))
    .bind((case.claimed || case.fence != Some(false)).then_some(NONCE))
    .bind(case.fence)
    .execute(pool)
    .await
    .expect("seed durable schedule facts");
}

async fn snapshot(pool: &PgPool) -> Value {
    schedule_absolute_correction_tests::state(pool).await
}

async fn runtime(pool: &PgPool) -> (Arc<CommandRuntime>, MockRest) {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = CommandRuntime::new(
        pool.clone(),
        executor_at(origin),
        router_with_commands(gates(true, true)),
        GUILD,
        true,
    );
    (runtime, mock)
}

fn safe_reply(reply: &Value) -> String {
    assert_eq!(reply["allowed_mentions"]["parse"], json!([]));
    let text = reply["content"]
        .as_str()
        .expect("text completion")
        .to_owned();
    assert!(text.encode_utf16().count() <= 2000, "UTF-16 reply bound");
    for private in [
        CLAIM,
        NONCE,
        "private-body-sentinel",
        "induced-private-store-error",
    ] {
        assert!(
            !text.contains(private),
            "private store fields must not reach replies"
        );
    }
    text
}

fn patches(mock: &MockRest) -> Vec<String> {
    mock.requests()
        .iter()
        .filter(|r| r.method == "PATCH")
        .map(|r| {
            let reply: Value = serde_json::from_slice(&r.body).expect("completion JSON");
            safe_reply(&reply)
        })
        .collect()
}

fn contains_any(text: &str, words: &[&str]) -> bool {
    words.iter().any(|word| text.contains(word))
}

fn row_for<'a>(content: &'a str, id: &str) -> &'a str {
    content
        .lines()
        .find(|line| line.starts_with(&format!("`{id}` ")))
        .unwrap_or_else(|| panic!("complete stable id {id} must lead one whole row: {content}"))
}

fn assert_snapshot(content: &str) {
    let lower = content.to_lowercase();
    assert!(
        lower.contains("snapshot"),
        "state is an observation, not a delivery promise: {content}"
    );
    assert!(
        contains_any(&lower, &["check", "inspect"]),
        "give a safe inspection action: {content}"
    );
}

fn assert_facts(content: &str, id: &str, case: &Case) {
    let row = row_for(content, id);
    let lower = row.to_lowercase();
    assert!(
        row.contains("every 60m"),
        "recurrence survives enrichment: {row}"
    );
    assert_eq!(
        lower.contains("(disabled)"),
        !case.enabled,
        "retain disabled state independently of delivery evidence: {row}"
    );
    match case.view {
        View::Ready => {
            assert!(
                row.contains("<t:"),
                "ordinary enabled rows retain localized due time: {row}"
            );
            assert!(
                contains_any(&lower, &["delayed", "pending", "due"]),
                "due is not guaranteed delivery: {row}"
            );
            assert!(
                !contains_any(&lower, &["lease", "fenced", "uncertain"]),
                "do not invent a hold: {row}"
            );
        }
        View::Disabled => {
            assert!(
                !contains_any(
                    &lower,
                    &[
                        "pending",
                        "ticker",
                        "delivery may be delayed",
                        "lease",
                        "fenced",
                        "uncertain"
                    ]
                ),
                "disabled rows have no ordinary pending-delivery hint: {row}"
            );
        }
        View::Claimed => {
            assert!(
                contains_any(&lower, &["claimed", "lease"]),
                "current claim survives projection even if disabled/restored: {row}"
            );
            assert!(
                !lower.contains("delivery may be delayed"),
                "lease horizon is not the ordinary due/delivery hint: {row}"
            );
            // A horizon may be displayed, but it must be explicitly called a lease.
            if row.contains("<t:") {
                assert!(
                    lower.contains("lease"),
                    "localized horizon must be labelled lease, not next delivery: {row}"
                );
            }
            assert!(
                contains_any(
                    &content.to_lowercase(),
                    &[
                        "not prove",
                        "not proof",
                        "not mean",
                        "may no longer",
                        "not necessarily"
                    ]
                ),
                "a stale claim does not prove an active worker: {content}"
            );
        }
        View::Fenced => {
            assert!(
                contains_any(&lower, &["fenc", "uncertain", "admitted"]),
                "fence wins over claim/enabled status, including restored rows: {row}"
            );
            assert!(
                !lower.contains("delivery may be delayed"),
                "fenced occurrence is not ordinary pending work: {row}"
            );
            let page = content.to_lowercase();
            assert!(
                contains_any(
                    &page,
                    &[
                        "before send",
                        "before a send",
                        "before sending",
                        "before post",
                        "before a post"
                    ]
                ),
                "fence is armed before send; it cannot prove failure: {content}"
            );
            assert!(
                contains_any(
                    &page,
                    &["may", "might", "does not", "not proof", "not prove"]
                ),
                "do not assert a definite send failure: {content}"
            );
        }
    }
    if !case.enabled {
        assert!(
            !lower.contains("pending") && !lower.contains("delivery may be delayed"),
            "disabled oracle applies even with stronger restored evidence: {row}"
        );
    }
    for overpromise in [
        "worker is active",
        "delivery failed permanently",
        "will be delivered at",
        "guaranteed delivery",
        "automatically resend",
    ] {
        assert!(
            !lower.contains(overpromise),
            "unsupported delivery claim: {row}"
        );
    }
}

// Kept visible to the two legacy receipt controls. Their old exact 'Cancelled'
// string is superseded, not their prefix, deletion, audit or validation oracles.
pub(super) fn assert_removed_receipt(text: &str, id: &str) {
    let lower = text.to_lowercase();
    assert!(
        text.contains(&format!("`{id}`")),
        "receipt names resolved stable id: {text}"
    );
    assert!(
        contains_any(&lower, &["removed", "deleted"]),
        "confirm only durable removal: {text}"
    );
    assert!(
        contains_any(&lower, &["definition", "future"]),
        "bound the scope of removal: {text}"
    );
    assert!(
        contains_any(
            &lower,
            &["already", "in-flight", "in flight", "admitted", "started"]
        ),
        "acknowledge previously admitted work: {text}"
    );
    assert!(
        contains_any(&lower, &["may", "might", "could", "can still"]),
        "an admitted delivery may still appear: {text}"
    );
    assert!(
        contains_any(&lower, &["appear", "arrive", "post", "deliver", "send"]),
        "name the possible channel effect: {text}"
    );
    assert!(
        contains_any(&lower, &["check", "inspect"]) && lower.contains("channel"),
        "bounded channel-inspection guidance: {text}"
    );
    for promise in [
        "cancelled.",
        "canceled.",
        "message will not",
        "no message will",
        "nothing will be sent",
        "successfully recalled",
        "cleanup succeeded",
        "delivery was cancelled",
        "delivery was canceled",
    ] {
        assert!(
            !lower.contains(promise),
            "must not promise absence, recall or cleanup: {text}"
        );
    }
    assert!(text.encode_utf16().count() <= 2000);
}

#[tokio::test]
async fn ready_and_disabled_control_retains_due_hint_without_mutation() {
    let db = fixture().await;
    seed(
        db.pool(),
        "ordinary",
        &CASES[0],
        CHANNEL_S,
        "2020-01-01T00:00:00.000Z",
    )
    .await;
    seed(db.pool(), "disabled", &CASES[4], CHANNEL_S, AT).await;
    let before = snapshot(db.pool()).await;
    let (runtime, mock) = runtime(db.pool()).await;
    invoke_schedule_list(&runtime, None).await;
    let after = snapshot(db.pool()).await;
    drop(runtime);
    db.close().await.expect("fixture teardown");
    let content = safe_reply(&mock.deferred_reply());
    assert_eq!(
        after, before,
        "successful list is entirely read-only and audit-free"
    );
    assert_facts(&content, "ordinary", &CASES[0]);
    assert_facts(&content, "disabled", &CASES[4]);
    assert_eq!(mock.requests().len(), 2, "defer and completion only");
    mock.shutdown().await;
}

#[tokio::test]
async fn first_window_exposes_claim_fence_precedence_and_restored_disabled_facts() {
    let db = fixture().await;
    for case in CASES {
        seed(db.pool(), case.name, case, CHANNEL_S, AT).await;
    }
    // Claim evidence must not disappear just because a restored timestamp is absent.
    sqlx::query("UPDATE scheduled_messages SET claimed_at = NULL WHERE id = 'disabled-claim'")
        .execute(db.pool())
        .await
        .expect("restore partial claim");
    let before = snapshot(db.pool()).await;
    let (runtime, mock) = runtime(db.pool()).await;
    invoke_schedule_list(&runtime, None).await;
    let mut contents = patches(&mock);
    let total = schedule_list_page_indicator(&contents[0]).map_or(1, |(_, total)| total);
    for page in 2..=total {
        invoke_schedule_list(&runtime, Some(page)).await;
    }
    contents = patches(&mock);
    let after = snapshot(db.pool()).await;
    drop(runtime);
    db.close().await.expect("fixture teardown");
    assert_eq!(
        after, before,
        "projecting facts must not clear claims/fences or write audits"
    );
    for content in &contents {
        assert_snapshot(content);
    }
    for case in CASES {
        let page = contents
            .iter()
            .find(|page| page.contains(&format!("`{}` ", case.name)))
            .expect("case listed");
        assert_facts(page, case.name, case);
    }
    assert_eq!(
        mock.requests().len(),
        contents.len() * 2,
        "list makes no channel/history calls"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn hidden_window_projects_all_facts_and_packs_complete_unicode_rows_with_overflow() {
    let db = fixture().await;
    let ids: Vec<String> = (0..52)
        .map(|n| format!("s{n:02}-{}", "😀".repeat(18)))
        .collect();
    for (n, id) in ids.iter().enumerate().rev() {
        let channel = if n == 26 {
            "1234`\n@everyone😀"
        } else {
            "1234567890123456789"
        };
        seed(db.pool(), id, &CASES[n % CASES.len()], channel, AT).await;
    }
    seed(db.pool(), "foreign-private-row", &CASES[2], CHANNEL_S, AT).await;
    sqlx::query("UPDATE scheduled_messages SET guild_id = '9999' WHERE id = 'foreign-private-row'")
        .execute(db.pool())
        .await
        .expect("foreign-guild fixture");
    let before = snapshot(db.pool()).await;
    let (runtime, mock) = runtime(db.pool()).await;
    invoke_schedule_list(&runtime, None).await;
    let first = patches(&mock);
    let first_total = schedule_list_page_indicator(&first[0])
        .expect("packed first window indicator")
        .1;
    for page in 2..=first_total {
        invoke_schedule_list(&runtime, Some(page)).await;
    }
    // The continuation after the bounded first window, not a fixed page number.
    invoke_schedule_list(&runtime, Some(first_total + 1)).await;
    let so_far = patches(&mock);
    let total = schedule_list_page_indicator(so_far.last().expect("hidden first page"))
        .expect("hidden global indicator")
        .1;
    for page in first_total + 2..=total {
        invoke_schedule_list(&runtime, Some(page)).await;
    }
    let contents = patches(&mock);
    let after = snapshot(db.pool()).await;
    drop(runtime);
    db.close().await.expect("fixture teardown");
    assert_eq!(before, after, "all windows preserve complete durable state");
    assert!(
        (first_total + 1..=50).contains(&total),
        "bounded hidden pages"
    );
    let listed: Vec<&str> = contents
        .iter()
        .flat_map(|page| schedule_list_row_lines(page))
        .map(schedule_list_row_id)
        .collect();
    assert_eq!(
        listed,
        ids[..50].iter().map(String::as_str).collect::<Vec<_>>(),
        "first and hidden windows expose exactly 25 whole rows each, in order"
    );
    for (n, content) in contents.iter().enumerate() {
        assert_eq!(
            schedule_list_page_indicator(content),
            Some((
                n as i64 + 1,
                if n < first_total as usize {
                    first_total
                } else {
                    total
                }
            ))
        );
        assert_snapshot(content);
        assert!(!content.contains("foreign-private-row"));
        assert!(
            !content.lines().any(|line| line.starts_with("@everyone")),
            "hostile fields cannot inject rows"
        );
        for line in schedule_list_row_lines(content) {
            let id = schedule_list_row_id(line);
            let index = ids
                .iter()
                .position(|candidate| candidate == id)
                .expect("known complete id");
            assert_facts(content, id, &CASES[index % CASES.len()]);
        }
    }
    let end_first = &contents[first_total as usize - 1];
    assert!(
        end_first.contains("(truncated, 27 more)")
            && end_first.contains(&format!("Continued on page {}.", first_total + 1)),
        "continuation and initial overflow remain reachable: {end_first}"
    );
    assert!(contents
        .last()
        .expect("hidden terminal page")
        .contains("(truncated, 2 more)"));
    assert_eq!(mock.requests().len(), contents.len() * 2);
    mock.shutdown().await;
}

#[tokio::test]
async fn list_reads_current_facts_after_defer_not_a_cached_ready_row() {
    let db = fixture().await;
    seed(db.pool(), "read-race", &CASES[0], CHANNEL_S, AT).await;
    let (mock, gate) = GatedRest::start_gated(
        vec![ScriptedResponse::json(200, json!({"id":"99"}))],
        ScriptedResponse::json(200, json!({"id":"99"})),
    )
    .await;
    let runtime = CommandRuntime::new(
        db.pool().clone(),
        executor_at(mock.origin()),
        router_with_commands(gates(true, true)),
        GUILD,
        true,
    );
    let command = tokio::spawn(async move {
        invoke_schedule_list(&runtime, None).await;
    });
    tokio::time::timeout(Duration::from_secs(10), gate.wait_for_request())
        .await
        .expect("defer request barrier");
    sqlx::query("UPDATE scheduled_messages SET claim_token = $1, delivery_uncertain = TRUE WHERE id = 'read-race'")
        .bind(CLAIM).execute(db.pool()).await.expect("arm facts before list read");
    let before = snapshot(db.pool()).await;
    gate.release();
    tokio::time::timeout(Duration::from_secs(10), command)
        .await
        .expect("bounded command")
        .expect("command task");
    let after = snapshot(db.pool()).await;
    db.close().await.expect("fixture teardown");
    let requests = mock.requests();
    let completion = requests
        .iter()
        .find(|r| r.method == "PATCH")
        .expect("completion");
    let reply: Value = serde_json::from_slice(&completion.body).expect("completion JSON");
    let content = safe_reply(&reply);
    assert_eq!(before, after, "read snapshot does not settle the new fence");
    assert_snapshot(&content);
    assert_facts(&content, "read-race", &CASES[3]);
    assert_eq!(requests.len(), 2);
    mock.shutdown().await;
}

async fn remove(runtime: &Arc<CommandRuntime>, prefix: &str) {
    tokio::time::timeout(
        Duration::from_secs(10),
        runtime.on_interaction(&slash(
            "schedule-remove",
            Some(CHANNEL),
            vec![option("id", CommandOptionValue::String(prefix.to_owned()))],
        )),
    )
    .await
    .expect("bounded remove completion");
}

#[tokio::test]
async fn successful_remove_confirms_definition_not_message_absence() {
    let db = fixture().await;
    seed(db.pool(), "remove-full-id", &CASES[0], CHANNEL_S, AT).await;
    let (runtime, mock) = runtime(db.pool()).await;
    remove(&runtime, "remove-full").await;
    let remaining = get_scheduled(db.pool(), GUILD_S, "remove-full-id")
        .await
        .expect("read removal");
    let state = snapshot(db.pool()).await;
    drop(runtime);
    db.close().await.expect("fixture teardown");
    let text = safe_reply(&mock.deferred_reply());
    assert!(remaining.is_none());
    assert_eq!(state["audits"].as_array().expect("audits").len(), 1);
    assert_eq!(state["audits"][0]["action"], "scheduled.delete");
    assert_eq!(state["audits"][0]["outcome"], "ok");
    assert_eq!(state["audits"][0]["target_key"], "remove-full-id");
    assert_removed_receipt(&text, "remove-full-id");
    assert!(
        text.contains("3333"),
        "guide inspection of the resolved destination channel: {text}"
    );
    assert_eq!(
        mock.requests().len(),
        2,
        "definition removal cannot assert remote absence by probing Discord"
    );
    mock.shutdown().await;
}

#[derive(Clone, Copy)]
enum SendResult {
    Success,
    CleanupRefused,
    Unknown,
}

struct Race {
    receipt: String,
    admitted: ScheduledMessageRow,
    removed: bool,
    audits: Vec<(String, String, Option<String>)>,
    posts: usize,
    deletes: usize,
    requests_before_release: usize,
}

async fn admitted_remove(result: SendResult) -> Race {
    let db = fixture().await;
    seed(
        db.pool(),
        "admitted-full-id",
        &CASES[0],
        CHANNEL_S,
        "2020-01-01T00:00:00.000Z",
    )
    .await;
    let response = ScriptedResponse::json(200, json!({"id":"9001"}));
    let response = if matches!(result, SendResult::Unknown) {
        response.truncated_body()
    } else {
        response
    };
    let cleanup = if matches!(result, SendResult::CleanupRefused) {
        403
    } else {
        204
    };
    // Separate command and send endpoints: the POST response is held, but the
    // slash receipt can complete. The real ticker owns all claims/fences/audits.
    let (sender, gate) =
        GatedRest::start_gated(vec![response], ScriptedResponse::status(cleanup)).await;
    let rest = executor_at(sender.origin());
    let pool = db.pool().clone();
    let ticker =
        tokio::spawn(
            async move { crate::scheduled_jobs::tick(&pool, &rest, GUILD_S, || T0).await },
        );
    tokio::time::timeout(Duration::from_secs(10), gate.wait_for_request())
        .await
        .expect("POST admitted barrier");
    let admitted = get_scheduled(db.pool(), GUILD_S, "admitted-full-id")
        .await
        .expect("read admitted state")
        .expect("definition held");
    let (runtime, mock) = runtime(db.pool()).await;
    remove(&runtime, "admitted-full").await;
    let receipt = safe_reply(&mock.deferred_reply());
    let removed = get_scheduled(db.pool(), GUILD_S, "admitted-full-id")
        .await
        .expect("read removed definition")
        .is_none();
    let requests_before_release = sender.requests().len();
    gate.release();
    tokio::time::timeout(Duration::from_secs(20), ticker)
        .await
        .expect("bounded tick")
        .expect("ticker task")
        .expect("tick");
    // A later tick cannot recreate/retry the removed definition.
    crate::scheduled_jobs::tick(db.pool(), &executor_at(sender.origin()), GUILD_S, || {
        T0 + 120_000
    })
    .await
    .expect("later tick");
    let audits = sqlx::query_as("SELECT action, outcome, reason FROM automation_audit_log WHERE guild_id = $1 ORDER BY created_at, id")
        .bind(GUILD_S).fetch_all(db.pool()).await.expect("read race audits");
    let requests = sender.requests();
    let race = Race {
        receipt,
        admitted,
        removed,
        audits,
        posts: requests
            .iter()
            .filter(|r| r.method == "POST" && r.path == "/api/v10/channels/3333/messages")
            .count(),
        deletes: requests
            .iter()
            .filter(|r| r.method == "DELETE" && r.path == "/api/v10/channels/3333/messages/9001")
            .count(),
        requests_before_release,
    };
    drop(runtime);
    mock.shutdown().await;
    sender.shutdown().await;
    db.close().await.expect("fixture teardown");
    race
}

fn assert_race_controls(race: &Race) {
    assert!(
        race.admitted.claim_token.is_some() && race.admitted.delivery_uncertain,
        "real scheduler arms the fence before the POST response"
    );
    assert!(
        race.removed,
        "definition removal completes while the response is held"
    );
    assert_eq!(
        race.requests_before_release, 1,
        "receipt precedes any orphan cleanup result"
    );
    assert_eq!(race.posts, 1, "no replay of removed definition");
    assert_eq!(
        race.audits
            .iter()
            .filter(|(action, outcome, _)| action == "scheduled.delete" && outcome == "ok")
            .count(),
        1
    );
}

#[tokio::test]
async fn remove_during_admitted_post_does_not_promise_successful_cleanup() {
    let race = admitted_remove(SendResult::Success).await;
    assert_race_controls(&race);
    assert_eq!(race.deletes, 1, "existing stale-send cleanup stays intact");
    assert!(race.audits.contains(&(
        "scheduled.run".to_owned(),
        "stale_completion".to_owned(),
        None
    )));
    assert_removed_receipt(&race.receipt, "admitted-full-id");
}

#[tokio::test]
async fn remove_during_admitted_post_warns_even_when_cleanup_is_refused() {
    let race = admitted_remove(SendResult::CleanupRefused).await;
    assert_race_controls(&race);
    assert_eq!(race.deletes, 1);
    assert!(race.audits.contains(&(
        "scheduled.run".to_owned(),
        "stale_completion".to_owned(),
        Some("discord_rejected".to_owned())
    )));
    assert_removed_receipt(&race.receipt, "admitted-full-id");
}

#[tokio::test]
async fn remove_during_unknown_post_outcome_does_not_claim_message_absence() {
    let race = admitted_remove(SendResult::Unknown).await;
    assert_race_controls(&race);
    assert_eq!(
        race.deletes, 0,
        "lost receipt supplies no usable message id for cleanup"
    );
    assert_removed_receipt(&race.receipt, "admitted-full-id");
}

#[tokio::test]
async fn missing_and_ambiguous_prefixes_remain_read_only_refusals_control() {
    let db = fixture().await;
    seed(db.pool(), "amb-111", &CASES[0], CHANNEL_S, AT).await;
    seed(db.pool(), "amb-222", &CASES[2], CHANNEL_S, AT).await;
    let before = snapshot(db.pool()).await;
    let (runtime, mock) = runtime(db.pool()).await;
    remove(&runtime, "amb").await;
    remove(&runtime, "missing").await;
    let after = snapshot(db.pool()).await;
    drop(runtime);
    db.close().await.expect("fixture teardown");
    assert_eq!(before, after);
    let contents = patches(&mock);
    assert_eq!(contents.len(), 2);
    for (text, prefix) in contents.iter().zip(["amb", "missing"]) {
        assert_eq!(text, &format!("No unique scheduled message matches `{prefix}`. Use the full id from /schedule-list."));
    }
    assert_eq!(mock.requests().len(), 4);
    mock.shutdown().await;
}

async fn read_failure(pattern: &str, page: i64) {
    let db = fixture().await;
    for n in 0..27 {
        seed(db.pool(), &format!("cap-{n:02}"), &CASES[0], CHANNEL_S, AT).await;
    }
    // Fail a specific real store read, not the executor or a fake list result.
    sqlx::query("CREATE FUNCTION fail_delivery_truth_read(pat text) RETURNS boolean AS $$ BEGIN IF current_query() LIKE pat THEN RAISE EXCEPTION 'induced-private-store-error'; END IF; RETURN TRUE; END; $$ LANGUAGE plpgsql VOLATILE")
        .execute(db.pool()).await.expect("install read fault");
    sqlx::query("ALTER TABLE scheduled_messages RENAME TO schedule_truth_base")
        .execute(db.pool())
        .await
        .expect("fault base");
    assert!(!pattern.contains('\''));
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE VIEW scheduled_messages AS SELECT * FROM schedule_truth_base WHERE fail_delivery_truth_read('{pattern}')")))
        .execute(db.pool()).await.expect("fault view");
    let before: Value =
        sqlx::query_scalar("SELECT jsonb_agg(to_jsonb(s) ORDER BY id) FROM schedule_truth_base s")
            .fetch_one(db.pool())
            .await
            .expect("rows before fault");
    let (runtime, mock) = runtime(db.pool()).await;
    invoke_schedule_list(&runtime, Some(page)).await;
    let after: Value =
        sqlx::query_scalar("SELECT jsonb_agg(to_jsonb(s) ORDER BY id) FROM schedule_truth_base s")
            .fetch_one(db.pool())
            .await
            .expect("rows after fault");
    let audits: Vec<(
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
    )> = sqlx::query_as(
        "SELECT action, outcome, target_key, reason, actor_id FROM automation_audit_log",
    )
    .fetch_all(db.pool())
    .await
    .expect("failure audit");
    drop(runtime);
    db.close().await.expect("fixture teardown");
    assert_eq!(before, after);
    let content = safe_reply(&mock.deferred_reply());
    assert_eq!(content, SCHEDULE_STORE_FAILURE_REPLY);
    assert_eq!(audits.len(), 1);
    let (action, outcome, target, reason, actor) = &audits[0];
    assert_schedule_store_failure_audit(
        action,
        outcome,
        reason.as_deref(),
        actor.as_deref(),
        "scheduled.list",
    );
    assert!(target.as_deref().is_none_or(str::is_empty));
    assert_eq!(mock.requests().len(), 2);
    mock.shutdown().await;
}

#[tokio::test]
async fn first_read_failure_retains_rejected_audit_control() {
    read_failure("%", 1).await;
}

#[tokio::test]
async fn hidden_read_failure_retains_rejected_audit_control() {
    read_failure("%OFFSET%", 3).await;
}

#[tokio::test]
async fn unknown_overflow_failure_retains_rejected_audit_control() {
    read_failure("%count(*)%", 3).await;
}

#[tokio::test]
async fn remove_storage_failure_is_not_a_successful_removal_control() {
    let db = fixture().await;
    seed(db.pool(), "delete-refused", &CASES[0], CHANNEL_S, AT).await;
    sqlx::query("CREATE FUNCTION fail_truth_delete() RETURNS trigger AS $$ BEGIN RAISE EXCEPTION 'induced-private-store-error'; END; $$ LANGUAGE plpgsql")
        .execute(db.pool()).await.expect("delete fault");
    sqlx::query("CREATE TRIGGER fail_truth_delete BEFORE DELETE ON scheduled_messages FOR EACH ROW EXECUTE FUNCTION fail_truth_delete()")
        .execute(db.pool()).await.expect("delete trigger");
    let before: Value =
        sqlx::query_scalar("SELECT jsonb_agg(to_jsonb(s) ORDER BY id) FROM scheduled_messages s")
            .fetch_one(db.pool())
            .await
            .expect("before delete");
    let (runtime, mock) = runtime(db.pool()).await;
    remove(&runtime, "delete-refused").await;
    let after: Value =
        sqlx::query_scalar("SELECT jsonb_agg(to_jsonb(s) ORDER BY id) FROM scheduled_messages s")
            .fetch_one(db.pool())
            .await
            .expect("after delete");
    let audits: Vec<(String, String)> =
        sqlx::query_as("SELECT action, outcome FROM automation_audit_log")
            .fetch_all(db.pool())
            .await
            .expect("refusal audit");
    drop(runtime);
    db.close().await.expect("fixture teardown");
    assert_eq!(before, after, "failed deletion keeps the definition");
    assert_eq!(
        safe_reply(&mock.deferred_reply()),
        SCHEDULE_STORE_FAILURE_REPLY
    );
    assert_eq!(
        audits,
        [("scheduled.delete".to_owned(), "rejected".to_owned())]
    );
    mock.shutdown().await;
}
