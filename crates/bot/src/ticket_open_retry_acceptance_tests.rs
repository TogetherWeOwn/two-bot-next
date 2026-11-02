//! Protected Open retry acceptance. Real supervisor/runtime/store/executor;
//! faults only at the disposable database and loopback Discord boundaries.
#![cfg(test)]

use super::*;
use std::collections::{BTreeMap, BTreeSet};
use two_bot_testsupport::TestDatabase;

// Observable safety envelopes, not pins on a private queue representation.
// Retry must become eligible within twelve existing five-minute passes.
const ELIGIBLE_PASSES: usize = 12;
const RETRY_PASS_CEILING: usize = 64;
const RETAINED_CEILING: usize = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fault {
    None,
    Channel,
    History,
    Slow,
    AmbiguousPost,
    WrongId,
    WrongGuild,
    WrongParent,
    WrongTopic,
    MissingTopic,
    WrongType,
    Public,
    MissingStaff,
    Unreadable,
    MalformedHistory,
}

struct RemoteRow {
    id: String,
    opener: String,
    fault: Fault,
    controls: bool,
}

#[derive(Default)]
struct Remote {
    rows: BTreeMap<String, RemoteRow>,
    panel_reads: usize,
}

struct Fixture {
    db: TestDatabase,
    mock: MockRest,
    tickets: Arc<TicketRuntime>,
    remote: Arc<TaskMutex<Remote>>,
    entered: Arc<tokio::sync::Notify>,
}

fn private_receipt(channel: &str, row: &RemoteRow) -> Value {
    let mut doc = json!({
        "id":channel,"guild_id":"100","type":0,"parent_id":"200",
        "topic":format!("two-ticket:{}", row.id),
        "name":"retry-channel-name-private-sentinel",
        "permission_overwrites":[
            {"id":"100","type":0,"allow":"0","deny":"1024"},
            {"id":"400","type":1,"allow":"68624","deny":"0"},
            {"id":row.opener,"type":1,"allow":"68608","deny":"0"},
            {"id":"300","type":0,"allow":"68608","deny":"0"}
        ]
    });
    match row.fault {
        Fault::WrongId => doc["id"] = json!("999"),
        Fault::WrongGuild => doc["guild_id"] = json!("101"),
        Fault::WrongParent => doc["parent_id"] = json!("201"),
        Fault::WrongTopic => doc["topic"] = json!("two-ticket:foreign"),
        Fault::MissingTopic => {
            doc.as_object_mut().unwrap().remove("topic");
        }
        Fault::WrongType => doc["type"] = json!(2),
        Fault::Public => doc["permission_overwrites"][0]["deny"] = json!("0"),
        Fault::MissingStaff => {
            doc["permission_overwrites"].as_array_mut().unwrap().pop();
        }
        Fault::Unreadable => doc["permission_overwrites"][1]["allow"] = json!("3088"),
        _ => {}
    }
    doc
}

fn controls_message() -> Value {
    json!({"id":"900","author":{"id":"400"},"components":[
        {"type":1,"components":[
            {"type":2,"custom_id":TICKET_CLAIM_ID},
            {"type":2,"custom_id":TICKET_CLOSE_ID}
        ]}
    ]})
}

impl Fixture {
    async fn new() -> Self {
        let url = std::env::var("TWO_TEST_DATABASE_URL").expect(
            "set TWO_TEST_DATABASE_URL to the documented disposable agent-testdb bootstrap",
        );
        let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("guarded disposable database");
        crate::gateway::ensure_crypto_provider();
        let remote = Arc::new(TaskMutex::new(Remote::default()));
        let served = Arc::clone(&remote);
        let entered = Arc::new(tokio::sync::Notify::new());
        let signal = Arc::clone(&entered);
        let mock = MockRest::with_responder(move |request| {
            let mut remote = served.lock().unwrap();
            match (request.method.as_str(), request.path.as_str()) {
                ("GET", "/api/v10/channels/700") => {
                    return ScriptedResponse::json(
                        200,
                        json!({
                            "id":"700","guild_id":"100","type":0,
                            "permission_overwrites":[
                                {"id":"400","type":1,"allow":"68624","deny":"0"}
                            ]
                        }),
                    )
                }
                ("GET", "/api/v10/channels/700/messages?limit=50") => {
                    remote.panel_reads += 1;
                    return ScriptedResponse::json(
                        200,
                        json!([{
                            "id":"901","author":{"id":"400"},"components":[
                                {"type":1,"components":[{"type":2,"custom_id":TICKET_OPEN_ID}]}
                            ]
                        }]),
                    );
                }
                _ => {}
            }
            let parts: Vec<_> = request.path.split('/').collect();
            let Some(channel) = parts.get(4) else {
                return ScriptedResponse::status(500);
            };
            let Some(row) = remote.rows.get_mut(*channel) else {
                return ScriptedResponse::status(500);
            };
            if request.method == "GET" && parts.len() == 5 {
                match row.fault {
                    Fault::Channel => return ScriptedResponse::status(503),
                    Fault::Slow => {
                        signal.notify_one();
                        return ScriptedResponse::json(200, private_receipt(channel, row))
                            .delayed(Duration::from_secs(60));
                    }
                    _ => return ScriptedResponse::json(200, private_receipt(channel, row)),
                }
            }
            if request.method == "GET" && parts.get(5) == Some(&"messages?limit=50") {
                if row.fault == Fault::History {
                    return ScriptedResponse::status(503);
                }
                if row.fault == Fault::MalformedHistory {
                    return ScriptedResponse::json(
                        200,
                        json!([{
                            "id":"902","author":{"id":"400"},
                            "components":"retry-transcript-member-content-sentinel"
                        }]),
                    );
                }
                return ScriptedResponse::json(
                    200,
                    if row.controls {
                        json!([controls_message()])
                    } else {
                        json!([])
                    },
                );
            }
            if request.method == "POST" && parts.get(5) == Some(&"messages") {
                row.controls = true;
                if row.fault == Fault::AmbiguousPost {
                    // Discord accepted the send but the client cannot prove it.
                    return ScriptedResponse::status(503);
                }
                return ScriptedResponse::json(200, json!({"id":"900"}));
            }
            ScriptedResponse::status(500)
        })
        .await;
        let tickets = runtime(db.pool().clone(), &mock);
        Self {
            db,
            mock,
            tickets,
            remote,
            entered,
        }
    }

    async fn row(&self, number: u64, fault: Fault, controls: bool) -> String {
        let id = format!("open-retry-{number:04}");
        let opener = (50_000 + number).to_string();
        let channel = (60_000 + number).to_string();
        assert!(matches!(
            self.tickets
                .store
                .reserve(&id, &opener, number as i64, 0)
                .await
                .unwrap(),
            OpenResult::Created(_)
        ));
        self.tickets
            .store
            .record_channel(&id, &channel)
            .await
            .unwrap();
        self.tickets.store.activate(&id, &channel).await.unwrap();
        self.remote.lock().unwrap().rows.insert(
            channel.clone(),
            RemoteRow {
                id,
                opener,
                fault,
                controls,
            },
        );
        channel
    }

    fn fault(&self, channel: &str, fault: Fault) {
        self.remote
            .lock()
            .unwrap()
            .rows
            .get_mut(channel)
            .unwrap()
            .fault = fault;
    }

    fn calls(&self, channel: &str, method: &str) -> usize {
        let prefix = format!("/api/v10/channels/{channel}");
        self.mock
            .requests()
            .iter()
            .filter(|r| {
                r.method == method
                    && (r.path == prefix || r.path.starts_with(&(prefix.clone() + "/")))
            })
            .count()
    }

    fn repaired(&self, channel: &str) -> bool {
        self.remote.lock().unwrap().rows[channel].controls
    }

    async fn pass(&self) {
        // Pause only between calls: SQL and loopback I/O run with real time.
        // Auto-advancing a paused runtime during SQL creates false timeouts.
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(300)).await;
        tokio::time::resume();
        let _ = self.tickets.recover().await;
    }

    async fn repair(&self, channel: &str) -> bool {
        for _ in 0..ELIGIBLE_PASSES {
            self.pass().await;
            if self.repaired(channel) {
                return true;
            }
        }
        false
    }

    async fn close(self) {
        self.mock.shutdown().await;
        self.db
            .close()
            .await
            .expect("verify disposable database teardown");
    }
}

// Baseline control: proves the SQL fixture, private receipt, history and POST
// observations work without relying on the new retry mechanism.
#[tokio::test]
async fn successful_open_control_attaches_once_and_unchanged_passes_do_no_row_work() {
    let f = Fixture::new().await;
    let channel = f.row(1, Fault::None, false).await;
    assert!(f.tickets.recover().await.is_ok());
    assert!(f.repaired(&channel));
    let reads = f.calls(&channel, "GET");
    for _ in 0..3 {
        f.pass().await;
    }
    let after = f.calls(&channel, "GET");
    let posts = f.calls(&channel, "POST");
    let mutation_methods: Vec<_> = f
        .mock
        .requests()
        .iter()
        .filter(|r| r.method != "GET")
        .map(|r| (r.method.clone(), r.path.clone()))
        .collect();
    f.close().await;
    assert!(reads >= 2, "channel receipt and history are actually read");
    assert_eq!(after, reads, "settled Open rows must not replay");
    assert_eq!(posts, 1);
    assert_eq!(
        mutation_methods,
        vec![(
            "POST".to_owned(),
            format!("/api/v10/channels/{channel}/messages")
        )]
    );
}

// Break caught: an error is logged after advancing the cursor, but no later
// eligible pass revisits the Open row. Successful rows must not be replayed.
#[tokio::test]
async fn transient_channel_and_history_failures_repair_once_without_replaying_successes() {
    let f = Fixture::new().await;
    let channel = f.row(1, Fault::Channel, false).await;
    let history = f.row(2, Fault::History, false).await;
    let settled = f.row(3, Fault::None, true).await;
    assert!(f.tickets.recover().await.is_err());
    let settled_reads = f.calls(&settled, "GET");
    assert!(settled_reads > 0, "positive control: later row was reached");
    assert!(
        f.remote.lock().unwrap().panel_reads > 0,
        "panel slot survives failed rows"
    );
    f.fault(&channel, Fault::None);
    f.fault(&history, Fault::None);
    let a = f.repair(&channel).await;
    let b = f.repair(&history).await;
    for _ in 0..3 {
        f.pass().await;
    }
    let channel_posts = f.calls(&channel, "POST");
    let history_posts = f.calls(&history, "POST");
    let later_reads = f.calls(&settled, "GET");
    f.close().await;
    assert!(
        a && b,
        "failed Open rows must regain eligibility behind the forward cursor"
    );
    assert_eq!(
        (channel_posts, history_posts),
        (1, 1),
        "exactly one controls attachment per repair"
    );
    assert_eq!(
        later_reads, settled_reads,
        "unchanged successful Open rows remain zero-work"
    );
}

// Break caught: successful REST recovery never happens after the earlier SQL
// transcript-existence probe failed before an action could be selected.
#[tokio::test]
async fn transcript_probe_failure_is_retryable_before_action_selection() {
    let f = Fixture::new().await;
    let channel = f.row(1, Fault::None, false).await;
    sqlx::query("ALTER TABLE ticket_transcripts RENAME TO retry_hidden_transcripts")
        .execute(f.db.pool())
        .await
        .unwrap();
    assert!(f.tickets.recover().await.is_err());
    assert_eq!(
        f.calls(&channel, "GET"),
        0,
        "probe failure must precede Discord"
    );
    sqlx::query("ALTER TABLE retry_hidden_transcripts RENAME TO ticket_transcripts")
        .execute(f.db.pool())
        .await
        .unwrap();
    let repaired = f.repair(&channel).await;
    let posts = f.calls(&channel, "POST");
    f.close().await;
    assert!(
        repaired,
        "transcript-probe failure must retain bounded Open retry eligibility"
    );
    assert_eq!(posts, 1);
}

// Break caught: advancing before await loses a cancelled row, or a competing
// pass overlaps it. The original runtime and lane survive cancellation.
#[tokio::test]
async fn cancellation_releases_lane_and_preserves_single_flight_retry() {
    let f = Fixture::new().await;
    let channel = f.row(1, Fault::Slow, false).await;
    let rt = Arc::clone(&f.tickets);
    let work = tokio::spawn(async move { rt.recover().await });
    tokio::time::timeout(Duration::from_secs(10), f.entered.notified())
        .await
        .expect("recovery reaches the faulted channel before cancellation");
    let before = f.calls(&channel, "GET");
    assert!(f.tickets.recover().await.is_ok());
    assert_eq!(
        f.calls(&channel, "GET"),
        before,
        "busy passes cannot overlap one row"
    );
    work.abort();
    assert!(work.await.unwrap_err().is_cancelled());
    assert!(f.tickets.lane.try_lock().is_ok());
    f.fault(&channel, Fault::None);
    let repaired = f.repair(&channel).await;
    let posts = f.calls(&channel, "POST");
    f.close().await;
    assert!(
        repaired,
        "scope cancellation must not strand an advanced Open row"
    );
    assert_eq!(posts, 1);
}

// Break caught: a per-item deadline drops the future but permanently removes
// eligibility, or consumes the panel/later-row budget.
#[tokio::test]
async fn timed_out_open_work_leaves_later_rows_and_panel_live_then_repairs() {
    let f = Fixture::new().await;
    let channel = f.row(1, Fault::Slow, false).await;
    let later = f.row(2, Fault::None, true).await;
    let rt = Arc::clone(&f.tickets);
    let work = tokio::spawn(async move { rt.recover().await });
    tokio::time::timeout(Duration::from_secs(10), f.entered.notified())
        .await
        .expect("recovery reaches the faulted channel before cancellation");
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(21)).await;
    tokio::time::resume();
    assert!(work.await.unwrap().is_err());
    assert!(
        f.calls(&later, "GET") > 0,
        "slow prefix cannot starve the next row"
    );
    assert!(f.remote.lock().unwrap().panel_reads > 0);
    f.fault(&channel, Fault::None);
    let repaired = f.repair(&channel).await;
    let posts = f.calls(&channel, "POST");
    f.close().await;
    assert!(
        repaired,
        "timed-out Open work must repair on a later eligible pass"
    );
    assert_eq!(posts, 1);
}

// Break caught: retry eligibility is treated as authorization instead of
// revalidating the original receipt and complete readable history.
#[tokio::test]
async fn due_retry_revalidates_identity_privacy_and_history_before_any_mutation() {
    let f = Fixture::new().await;
    let mut cases = Vec::new();
    for (i, fault) in [
        Fault::WrongId,
        Fault::WrongGuild,
        Fault::WrongParent,
        Fault::WrongTopic,
        Fault::MissingTopic,
        Fault::WrongType,
        Fault::Public,
        Fault::MissingStaff,
        Fault::Unreadable,
        Fault::MalformedHistory,
    ]
    .into_iter()
    .enumerate()
    {
        let channel = f.row(i as u64 + 1, Fault::None, false).await;
        cases.push((channel, fault));
    }
    sqlx::query("ALTER TABLE ticket_transcripts RENAME TO retry_hidden_transcripts")
        .execute(f.db.pool())
        .await
        .unwrap();
    let _ = f.tickets.recover().await;
    sqlx::query("ALTER TABLE retry_hidden_transcripts RENAME TO ticket_transcripts")
        .execute(f.db.pool())
        .await
        .unwrap();
    let before: Vec<_> = cases.iter().map(|(c, _)| f.calls(c, "GET")).collect();
    for (channel, fault) in &cases {
        f.fault(channel, *fault);
    }
    for _ in 0..ELIGIBLE_PASSES {
        f.pass().await;
    }
    let observed: Vec<_> = cases
        .iter()
        .zip(before)
        .map(|((c, fault), n)| {
            (
                *fault,
                f.calls(c, "GET") > n,
                f.calls(c, "POST"),
                f.calls(c, "DELETE"),
            )
        })
        .collect();
    let rows = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM tickets WHERE status='open'")
        .fetch_one(f.db.pool())
        .await
        .unwrap();
    let transcripts = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM ticket_transcripts")
        .fetch_one(f.db.pool())
        .await
        .unwrap();
    f.close().await;
    for (fault, attempted, posts, deletes) in observed {
        assert!(
            attempted,
            "{fault:?}: refusal must be exercised on an actual due retry"
        );
        assert_eq!(
            (posts, deletes),
            (0, 0),
            "{fault:?}: due does not mean authorized"
        );
    }
    assert_eq!(
        rows, 10,
        "unproven receipts cannot activate a terminal workflow"
    );
    assert_eq!(
        transcripts, 0,
        "reattachment cannot invent transcript content"
    );
}

// Break caught: retry blindly repeats a send whose outcome was ambiguous,
// ignoring the bot-authored controls already visible in proven history.
#[tokio::test]
async fn ambiguous_prior_send_is_resolved_from_history_without_duplicate_controls() {
    let f = Fixture::new().await;
    let channel = f.row(1, Fault::AmbiguousPost, false).await;
    assert!(f.tickets.recover().await.is_err());
    assert!(
        f.repaired(&channel),
        "remote accepted the first ambiguous send"
    );
    let before = f.calls(&channel, "GET");
    let first_posts = f.calls(&channel, "POST");
    f.fault(&channel, Fault::History);
    for _ in 0..3 {
        f.pass().await;
    }
    assert_eq!(
        f.calls(&channel, "POST"),
        first_posts,
        "unreadable history cannot authorize another send"
    );
    f.fault(&channel, Fault::None);
    for _ in 0..ELIGIBLE_PASSES {
        f.pass().await;
    }
    let checked = f.calls(&channel, "GET") > before;
    let posts = f.calls(&channel, "POST");
    f.close().await;
    assert!(
        checked,
        "ambiguous result must be reconciled on a later pass"
    );
    assert_eq!(
        posts, 1,
        "one accepted send, even across ambiguous-result retries"
    );
}

// Break caught: no backoff causes Ready/recover bursts to hammer the failing
// row, or an unbounded retry backlog starves new work and the panel slot.
#[tokio::test]
async fn retry_backoff_capacity_and_per_pass_work_are_bounded_with_forward_fairness() {
    let f = Fixture::new().await;
    for i in 1..=257 {
        f.row(i, Fault::None, false).await;
    }
    sqlx::query("ALTER TABLE ticket_transcripts RENAME TO retry_hidden_transcripts")
        .execute(f.db.pool())
        .await
        .unwrap();
    let _ = f.tickets.recover().await;
    sqlx::query("ALTER TABLE retry_hidden_transcripts RENAME TO ticket_transcripts")
        .execute(f.db.pool())
        .await
        .unwrap();
    // Immediate recover/Ready bursts must not consume newly deferred work.
    for _ in 0..4 {
        let _ = f.tickets.recover().await;
    }
    let immediate = f
        .mock
        .requests()
        .iter()
        .filter(|r| r.path.starts_with("/api/v10/channels/60"))
        .count();
    let forward = f.row(258, Fault::None, false).await;
    let mut retained = BTreeSet::new();
    let mut per_pass = Vec::new();
    for _ in 0..ELIGIBLE_PASSES {
        let start = f.mock.requests().len();
        f.pass().await;
        let attempted: BTreeSet<_> = f.mock.requests()[start..]
            .iter()
            .filter(|r| r.method == "GET" && !r.path.ends_with("/messages?limit=50"))
            .filter_map(|r| r.path.strip_prefix("/api/v10/channels/"))
            .filter(|c| c.starts_with("60") && *c != forward)
            .map(str::to_owned)
            .collect();
        per_pass.push(attempted.len());
        retained.extend(attempted);
    }
    let forward_repaired = f.repaired(&forward);
    let panels = f.remote.lock().unwrap().panel_reads;
    f.close().await;
    assert_eq!(
        immediate, 0,
        "positive backoff prevents an immediate replay storm"
    );
    assert!(
        !retained.is_empty(),
        "capacity bounds cannot be implemented by discarding every failure"
    );
    assert!(
        retained.len() <= RETAINED_CEILING,
        "retry retention is finite: {} rows",
        retained.len()
    );
    assert!(
        per_pass.iter().all(|n| *n <= RETRY_PASS_CEILING),
        "retry work must be capped per pass: {per_pass:?}"
    );
    assert!(
        forward_repaired && panels >= ELIGIBLE_PASSES,
        "retry pressure cannot starve new rows or panels"
    );
}

// Break caught: stale eligibility outlives a terminal or removed row, causing
// delayed control posting after close/purge. A healthy row is the positive
// control proving the later passes did run.
#[tokio::test]
async fn closed_and_removed_rows_cancel_pending_reattachment() {
    let f = Fixture::new().await;
    let closed = f.row(1, Fault::Channel, false).await;
    let removed = f.row(2, Fault::Channel, false).await;
    let _ = f.tickets.recover().await;
    sqlx::query("UPDATE tickets SET status='closed', closed_at='2026-01-01T00:00:00Z' WHERE id='open-retry-0001'")
        .execute(f.db.pool()).await.unwrap();
    sqlx::query("DELETE FROM tickets WHERE id='open-retry-0002'")
        .execute(f.db.pool())
        .await
        .unwrap();
    f.fault(&closed, Fault::None);
    f.fault(&removed, Fault::None);
    let before = (f.calls(&closed, "GET"), f.calls(&removed, "GET"));
    let live = f.row(3, Fault::None, false).await;
    for _ in 0..ELIGIBLE_PASSES {
        f.pass().await;
    }
    let after = (f.calls(&closed, "GET"), f.calls(&removed, "GET"));
    let posts = (f.calls(&closed, "POST"), f.calls(&removed, "POST"));
    let repaired = f.repaired(&live);
    f.close().await;
    assert!(repaired, "positive control: new rows remain live");
    assert_eq!(
        after, before,
        "terminal/deleted rows must shed all retry work"
    );
    assert_eq!(
        posts,
        (0, 0),
        "a stale queue entry cannot reattach controls"
    );
}

// Break caught: only a manual repair/restart helps; existing supervisor ticks
// do not drive the retained work. There is exactly one supervisor start.
#[tokio::test]
async fn existing_supervisor_periodic_tick_repairs_without_another_ready_or_restart() {
    let f = Fixture::new().await;
    let channel = f.row(1, Fault::History, false).await;
    let supervisor = f.tickets.start().unwrap();
    f.tickets.on_ready(400);
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if f.remote.lock().unwrap().panel_reads > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("initial supervisor pass reaches panel");
    // Taking and releasing the lane is a completion fence for that pass.
    drop(f.tickets.lane.lock().await);
    f.fault(&channel, Fault::None);
    for _ in 0..ELIGIBLE_PASSES {
        let panels = f.remote.lock().unwrap().panel_reads;
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(300)).await;
        tokio::time::resume();
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if f.remote.lock().unwrap().panel_reads > panels {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("existing periodic supervisor reaches panel");
        drop(f.tickets.lane.lock().await);
        if f.repaired(&channel) {
            break;
        }
    }
    let repaired = f.repaired(&channel);
    let posts = f.calls(&channel, "POST");
    supervisor.shutdown().await;
    f.close().await;
    assert!(
        repaired,
        "the existing periodic supervisor must repair without restart or another Ready"
    );
    assert_eq!(posts, 1);
}

// Break caught: retry is operationally opaque or leaks receipt/history/member
// text. Field names are an AX contract; particular reason values remain the
// builder's choice, but must be bounded machine tokens, not error prose.
#[tokio::test]
async fn retry_diagnostics_are_actionable_bounded_and_content_free() {
    let capture = crate::tracing_capture::global();
    let start = capture.text().len();
    let f = Fixture::new().await;
    let channel = f.row(1, Fault::History, false).await;
    let _ = f.tickets.recover().await;
    f.fault(&channel, Fault::None);
    let _ = f.repair(&channel).await;
    f.close().await;
    let logs = capture.text();
    let lines: Vec<_> = logs[start..]
        .lines()
        .filter(|line| line.contains("ticket_id=open-retry-0001") && line.contains("retryable="))
        .collect();
    assert!(
        !lines.is_empty(),
        "retry diagnostics need ticket identity, retryable and next_action"
    );
    assert!(
        lines.len() <= 32,
        "one row cannot produce an unbounded retry log burst"
    );
    for line in lines {
        assert!(
            line.contains("next_action=") && line.contains("reason=") && line.contains("outcome="),
            "retry diagnostic needs stable reason/outcome and remediation: {line}"
        );
        for field in ["reason=", "outcome=", "next_action="] {
            let value = line
                .split(field)
                .nth(1)
                .unwrap()
                .split_whitespace()
                .next()
                .unwrap()
                .trim_matches('"');
            assert!(
                !value.is_empty()
                    && value.len() <= 64
                    && value
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                "bounded machine token: {line}"
            );
        }
    }
    for secret in [
        "retry-channel-name-private-sentinel",
        "retry-transcript-member-content-sentinel",
    ] {
        assert!(
            !logs[start..].contains(secret),
            "retry diagnostics cannot carry member/channel content"
        );
    }
}
