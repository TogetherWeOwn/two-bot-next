//! Protected edge cases for the close-repair acceptance target; not a production adapter.
use super::*;
use std::sync::Mutex;
use two_bot_core::lfg::LfgSelectAction;

#[tokio::test]
async fn unclassified_404_does_not_discard_pending_edit_as_definitive_absence() {
    let f = Fixture::new(403).await;
    f.seed(&post("repair-unclassified", Some("5001"), FUTURE))
        .await;
    f.close("repair-unclassified").await.expect("close");
    f.status.store(498, Ordering::SeqCst);
    let first = f.tick(clock(), 20).await;
    let attempted = edits(&f.rest.requests()).len();
    f.status.store(200, Ordering::SeqCst);
    let second = f.tick(clock() + Duration::days(1), 20).await;
    let requests = f.finish().await;
    assert!(
        first > 0 && attempted == 2,
        "RED: exercise the unclassified remote refusal"
    );
    assert!(
        second > 0,
        "unreadable 404 is not proof of a missing message"
    );
    assert_eq!(edits(&requests).len(), 3);
    assert_only_closed_edits(&requests, &["5001"]);
}

#[tokio::test]
async fn failed_pending_row_does_not_starve_later_rows_in_the_same_batch() {
    let f = Fixture::new(403).await;
    for (id, message) in [
        ("repair-failed-first", "5001"),
        ("repair-healthy-second", "5002"),
    ] {
        f.seed(&post(id, Some(message), FUTURE)).await;
        f.close(id).await.expect("close");
    }
    f.status.store(497, Ordering::SeqCst);
    let before = f.rest.requests().len();
    f.tick(clock(), 2).await;
    let first = f.rest.requests();
    f.status.store(200, Ordering::SeqCst);
    f.tick(clock() + Duration::days(1), 2).await;
    let requests = f.finish().await;
    let batch = edits(&first[before..]);
    assert_eq!(
        batch.len(),
        2,
        "RED: one failed pending edit must not abort the batch"
    );
    assert!(batch.iter().any(|r| r.path.ends_with("/messages/5002")));
    assert_eq!(
        edits(&requests)
            .iter()
            .filter(|r| r.path.ends_with("/messages/5002"))
            .count(),
        2,
        "settled healthy row must not be retried"
    );
    assert_eq!(
        edits(&requests)
            .iter()
            .filter(|r| r.path.ends_with("/messages/5001"))
            .count(),
        3,
        "failed row remains retryable"
    );
    assert_only_closed_edits(&requests, &["5001", "5002"]);
}

#[tokio::test]
async fn erased_post_is_not_recreated_or_refreshed_from_a_dangling_pending_fact() {
    let f = Fixture::new(403).await;
    f.seed(&post("repair-erased", Some("5001"), FUTURE)).await;
    f.close("repair-erased").await.expect("close");
    let retained = f.roster("repair-erased").await;
    let deleted = store::delete_lfg(f.db.pool(), GUILD, "repair-erased").await;
    f.status.store(200, Ordering::SeqCst);
    let before = f.rest.requests().len();
    f.tick(clock(), 20).await;
    f.tick(clock() + Duration::days(1), 20).await;
    let missing = store::get_lfg(f.db.pool(), GUILD, "repair-erased").await;
    let requests = f.finish().await;
    assert!(!retained.1.is_empty(), "nonempty erasure fixture");
    assert!(deleted.expect("authorized fixture erasure"));
    assert!(missing.expect("post lookup").is_none());
    assert_eq!(
        requests.len(),
        before,
        "erasure must remove repair authority too"
    );
    assert_only_closed_edits(&requests, &["5001"]);
}

#[tokio::test]
async fn failed_database_scan_preserves_pending_intent_for_a_later_restart() {
    let f = Fixture::new(403).await;
    f.seed(&post("repair-scan-failure", Some("5001"), FUTURE))
        .await;
    f.close("repair-scan-failure").await.expect("close");
    let closed = f.saved("repair-scan-failure").await;
    let before = f.rest.requests().len();
    // Fault only the test-owned database. The subsequent restart must not turn
    // an unreadable candidate scan into successful settlement or an empty store.
    sqlx::query("ALTER TABLE lfg_posts RENAME TO repair_scan_fault")
        .execute(f.db.pool())
        .await
        .expect("install fixture read failure");
    let failure = f.tick_with(&f.executor, GUILD, BOT, clock(), 20).await;
    sqlx::query("ALTER TABLE repair_scan_fault RENAME TO lfg_posts")
        .execute(f.db.pool())
        .await
        .expect("restore fixture table");
    let failed_requests = f.rest.requests().len();
    f.status.store(200, Ordering::SeqCst);
    let repaired = f.tick(clock() + Duration::days(1), 20).await;
    let after = f.saved("repair-scan-failure").await;
    let requests = f.finish().await;
    assert!(
        failure.is_err(),
        "unavailable scan must not report an empty successful pass"
    );
    assert_eq!(failed_requests, before);
    assert_eq!(after, closed);
    assert!(
        repaired > 0,
        "RED: a database outage cannot consume pending repair"
    );
    assert_eq!(edits(&requests).len(), 2);
    assert_only_closed_edits(&requests, &["5001"]);
}

async fn wait_for_advisory_contender(pool: &sqlx::PgPool) -> bool {
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let waiting: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM pg_locks l JOIN pg_stat_activity a ON a.pid = l.pid
                 WHERE a.datname = current_database() AND l.locktype = 'advisory' AND NOT l.granted
                 AND cardinality(pg_blocking_pids(l.pid)) > 0)",
            )
            .fetch_one(pool)
            .await
            .expect("observe fixture lock contention");
            if waiting {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .is_ok()
}

#[tokio::test]
async fn concurrent_signup_refresh_cannot_restore_open_controls_after_close_and_repair() {
    let f = Fixture::new(200).await;
    f.seed(&post("repair-refresh-race", Some("5001"), FUTURE))
        .await;
    let (mock, gate) = MockRest::start_gated(
        vec![ScriptedResponse::json(200, json!({"id": "5001"}))],
        ScriptedResponse::status(403),
    )
    .await;
    let executor =
        ActionExecutor::with_proxy(TOKEN.into(), Some(mock.origin())).expect("gated executor");
    let peer =
        f.db.independent_pool()
            .await
            .expect("contending close pool");
    let first = tokio::spawn({
        let service = LfgInteractions::new(f.db.pool().clone());
        let executor = executor.clone();
        async move {
            service
                .execute(
                    &executor,
                    LfgRequest::Select(LfgSelectAction::Signup {
                        post_id: "repair-refresh-race".into(),
                        role_key: "healer".into(),
                    }),
                    GUILD,
                    "3335",
                    89008,
                    BOT,
                )
                .await
        }
    });
    let arrived = tokio::time::timeout(std::time::Duration::from_secs(3), gate.wait_for_request())
        .await
        .is_ok();
    let second = tokio::spawn({
        let service = LfgInteractions::new(peer.clone());
        let executor = executor.clone();
        async move {
            service
                .execute(
                    &executor,
                    LfgRequest::Close {
                        post_id: "repair-refresh-race".into(),
                    },
                    GUILD,
                    OWNER,
                    89009,
                    BOT,
                )
                .await
        }
    });
    let waiting = wait_for_advisory_contender(f.db.pool()).await;
    let during = f.saved("repair-refresh-race").await;
    let premature = edits(&mock.requests()).len();
    // Always release/join before assertions, including a broken-lock RED path.
    gate.release();
    let signup = first.await.expect("signup task");
    let close = second.await.expect("close task");
    let closed = f.saved("repair-refresh-race").await;
    let roster = f.roster("repair-refresh-race").await;
    let prior_requests = mock.requests();
    mock.shutdown().await;
    drop(executor);
    peer.close().await;
    let repaired = f.tick(clock(), 20).await;
    let after = f.saved("repair-refresh-race").await;
    let retained = f.roster("repair-refresh-race").await;
    let requests = f.finish().await;
    assert!(
        arrived && waiting,
        "exercise real cross-pool contention, not task ordering"
    );
    assert_eq!(during.status, LfgStatus::Open);
    assert_eq!(
        premature, 1,
        "close waits through an in-flight open refresh"
    );
    assert!(signup.expect("saved signup").contains("joined"));
    assert!(close
        .expect("saved close warning")
        .to_lowercase()
        .contains("refresh"));
    assert_eq!(closed.status, LfgStatus::Closed);
    assert_eq!(roster.1.len(), 2);
    assert_eq!(after, closed);
    assert_eq!(retained, roster);
    let ordered = edits(&prior_requests);
    assert_eq!(ordered.len(), 2);
    assert!(
        !serde_json::from_slice::<Value>(&ordered[0].body).expect("open edit")["components"]
            .as_array()
            .expect("controls")
            .is_empty()
    );
    assert_only_closed_edits(&prior_requests[1..], &["5001"]);
    assert!(
        repaired > 0,
        "RED: concurrent saved closure must still be repairable"
    );
    assert_eq!(edits(&requests).len(), 1);
    assert_only_closed_edits(&requests, &["5001"]);
}

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);
impl std::io::Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("log capture").extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Process-wide log capture for this test binary.
///
/// A scoped `with_subscriber` capture can miss the repair diagnostic:
/// `tracing-core` resolves a callsite's first registration against the
/// registering thread's default while only one dispatcher is live, so a
/// sibling test that reaches the same `warn!` callsite first can cache
/// no-interest for it and the scoped subscriber is then never consulted.
/// One global install keeps callsite interest stable for the whole binary;
/// the logging test reads only the bytes appended after its own start
/// offset and attributes the classified event by its unique post id.
fn global_capture() -> Capture {
    static LOG: std::sync::OnceLock<Capture> = std::sync::OnceLock::new();
    LOG.get_or_init(|| {
        let capture = Capture::default();
        let writer = capture.clone();
        tracing::subscriber::set_global_default(
            tracing_subscriber::fmt()
                .json()
                .without_time()
                .with_ansi(false)
                .with_max_level(tracing::Level::INFO)
                .with_writer(move || writer.clone())
                .finish(),
        )
        .expect("install global log capture");
        capture
    })
    .clone()
}

#[tokio::test]
async fn failed_repair_logs_a_bounded_outcome_class_without_member_sql_or_remote_bodies() {
    let f = Fixture::new(403).await;
    let mut row = post("repair-logging", Some("5001"), FUTURE);
    row.title = "MEMBER_TITLE_SENTINEL".into();
    f.seed(&row).await;
    f.close(&row.id).await.expect("close");
    let capture = global_capture();
    let start = capture.0.lock().expect("log capture").len();
    let processed = f.tick(clock(), 20).await;
    let text = {
        let bytes = capture.0.lock().expect("capture");
        String::from_utf8_lossy(&bytes[start.min(bytes.len())..]).into_owned()
    };
    f.finish().await;
    assert!(!text.contains("MEMBER_TITLE_SENTINEL"));
    // The global window can also carry sibling tests' events (including
    // sqlx slow-statement warnings that legitimately quote SQL), so the
    // remaining content bans and the classification run on the diagnostics
    // carrying this post's unique id, under any field name. The
    // member-title sentinel stays window-wide: only this fixture can
    // source it.
    let diagnostics: Vec<Value> = text
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|event| {
            event["fields"].as_object().is_some_and(|fields| {
                fields
                    .values()
                    .any(|value| value.as_str() == Some(row.id.as_str()))
            })
        })
        .collect();
    let diagnostic_text = serde_json::to_string(&diagnostics).expect("diagnostics serialize");
    assert!(!diagnostic_text.contains("REMOTE_BODY_SENTINEL"));
    assert!(!diagnostic_text.contains("SELECT ") && !diagnostic_text.contains("UPDATE "));
    let classified = diagnostics.iter().any(|event| {
        let fields = &event["fields"];
        let outcome = fields.get("outcome").and_then(Value::as_str);
        let class = ["error_class", "cause", "code"]
            .iter()
            .find_map(|key| fields.get(*key).and_then(Value::as_str));
        [outcome, class].iter().all(|value| {
            value.is_some_and(|value| {
                !value.is_empty()
                    && value.len() <= 64
                    && value
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
            })
        })
    });
    assert!(processed > 0, "RED: repair attempt must actually execute");
    assert!(
        classified,
        "recovery needs content-free bounded outcome and error classification"
    );
}
