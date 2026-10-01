use super::*;
use crate::mock_rest::{MockRest, ScriptedResponse};
use serde_json::json;
use sqlx::postgres::PgPoolOptions;

fn runtime(mock: &MockRest) -> Arc<TicketRuntime> {
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://agent_test@agent-testdb:5432/agent_test")
        .unwrap();
    let runtime = Arc::new(
        TicketRuntime::new(
            pool,
            ActionExecutor::with_proxy("ticket-test-token".into(), Some(mock.origin())).unwrap(),
            TicketConfig {
                guild_id: "100".into(),
                category_id: "200".into(),
                panel_channel_id: "700".into(),
                staff_role_id: "300".into(),
                cooldown_seconds: COOLDOWN_SECONDS,
            },
        )
        .unwrap(),
    );
    runtime.set_bot_id(400);
    runtime
}

fn open_button() -> Interaction {
    serde_json::from_value(json!({
        "application_id":"400", "authorizing_integration_owners":{"0":"100"},
        "id":"800", "token":"ticket-test-token", "type":3, "version":1,
        "guild_id":"100", "data":{"custom_id":TICKET_OPEN_ID,"component_type":2},
        "member":{"roles":[],"permissions":"0","deaf":false,"mute":false,"flags":0,
            "user":{"id":"500","username":"member","discriminator":"0","avatar":null}}
    }))
    .unwrap()
}

async fn settle() {
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test(start_paused = true)]
async fn admission_and_cumulative_work_deadlines_release_the_guild_lane() {
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let runtime = runtime(&mock);
    let (started, receiver) = tokio::sync::oneshot::channel();
    let slow = Arc::clone(&runtime);
    // Exercise the same cumulative deadline used by execute: individually
    // successful slow pages must not keep this lane for the token's lifetime.
    let work = tokio::spawn(async move {
        bounded(BUTTON_TIMEOUT, async {
            let _guard = slow.lane.lock().await;
            started.send(()).unwrap();
            for _ in 0..100 {
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
            Ok(())
        })
        .await
    });
    receiver.await.unwrap();
    let unrelated = Arc::clone(&runtime);
    let waiting =
        tokio::spawn(async move { unrelated.execute(&open_button(), TicketAction::Open).await });
    settle().await;
    assert!(
        runtime.recover().await.is_ok(),
        "busy recovery must not overlap"
    );
    tokio::time::advance(LANE_WAIT_TIMEOUT).await;
    settle().await;
    assert!(matches!(waiting.await.unwrap(), Err(Failure::Timeout)));
    assert!(mock.requests().is_empty(), "failed admission cannot mutate");
    tokio::time::advance(BUTTON_TIMEOUT - LANE_WAIT_TIMEOUT).await;
    settle().await;
    assert!(matches!(work.await.unwrap(), Err(Failure::Timeout)));
    assert!(
        runtime.lane.try_lock().is_ok(),
        "cancelled work releases admission"
    );
    mock.shutdown().await;
}

fn recovery_ticket(id: u64) -> Ticket {
    Ticket {
        id: id.to_string(),
        guild_id: "100".into(),
        channel_id: Some((600 + id).to_string()),
        opener_id: "500".into(),
        claimed_by: None,
        status: TicketStatus::Open,
        created_at: id as i64,
        closing_started_at: None,
        closed_at: None,
    }
}

#[tokio::test(start_paused = true)]
async fn slow_recovery_prefix_rotates_and_leaves_a_panel_slot_on_each_pass() {
    let cursor = TaskMutex::new(None);
    let attempted = Arc::new(TaskMutex::new(Vec::new()));
    let mut panels = 0;
    for _ in 0..2 {
        let attempted = Arc::clone(&attempted);
        let started = Instant::now();
        let failed = recovery_batch(
            &cursor,
            (1..=15).map(recovery_ticket).collect(),
            move |ticket| {
                let attempted = Arc::clone(&attempted);
                async move {
                    attempted.lock().unwrap().push(ticket.id);
                    // Every stable prefix row is slower than its individual budget.
                    tokio::time::sleep(Duration::from_secs(50)).await;
                    Ok(())
                }
            },
        )
        .await;
        assert!(matches!(failed, Some(ErrorClass::Timeout)));
        bounded(PANEL_TIMEOUT, async {
            panels += 1;
            Ok(())
        })
        .await
        .ok()
        .unwrap();
        assert!(started.elapsed() < RECOVERY_TIMEOUT);
    }
    assert_eq!(panels, 2);
    let attempted = attempted.lock().unwrap();
    for id in 1..=15 {
        assert!(
            attempted.contains(&id.to_string()),
            "later row must not starve"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn cancelled_recovery_resumes_after_the_in_flight_row_even_if_it_disappears() {
    let cursor = Arc::new(TaskMutex::new(None));
    let started = Arc::new(tokio::sync::Notify::new());
    let task_cursor = Arc::clone(&cursor);
    let signal = Arc::clone(&started);
    let task = tokio::spawn(async move {
        recovery_batch(
            &task_cursor,
            (1..=3).map(recovery_ticket).collect(),
            move |_| {
                let signal = Arc::clone(&signal);
                async move {
                    signal.notify_one();
                    std::future::pending().await
                }
            },
        )
        .await
    });
    started.notified().await;
    task.abort();
    let _ = task.await;
    let attempted = TaskMutex::new(Vec::new());
    recovery_batch(
        &cursor,
        (2..=3).map(recovery_ticket).collect(),
        |ticket| async {
            attempted.lock().unwrap().push(ticket.id);
            Ok(())
        },
    )
    .await;
    assert_eq!(*attempted.lock().unwrap(), vec!["2", "3"]);
}

#[tokio::test(start_paused = true)]
async fn sub_millisecond_import_order_cannot_starve_decoded_cursor_rows() {
    let cursor = TaskMutex::new(None);
    let attempted = Arc::new(TaskMutex::new(Vec::new()));
    // Full database instants can put these rows in any order within one ms.
    let ids = [9, 8, 7, 6, 5, 4, 3, 2, 1, 15, 14, 13, 12, 11, 10];
    for _ in 0..2 {
        let attempted = Arc::clone(&attempted);
        recovery_batch(
            &cursor,
            ids.into_iter()
                .map(|id| {
                    let mut ticket = recovery_ticket(id);
                    ticket.created_at = 0;
                    ticket
                })
                .collect(),
            move |ticket| {
                let attempted = Arc::clone(&attempted);
                async move {
                    attempted.lock().unwrap().push(ticket.id);
                    tokio::time::sleep(Duration::from_secs(50)).await;
                    Ok(())
                }
            },
        )
        .await;
    }
    let attempted = attempted.lock().unwrap();
    for id in ids {
        assert!(
            attempted.contains(&id.to_string()),
            "decoded-key row must not starve"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn panel_slot_admits_five_slow_successful_calls_inside_supervisor_budget() {
    let started = Instant::now();
    let result = bounded(PANEL_TIMEOUT, async {
        // Ordinary panels need channel, member, roles, history and POST calls.
        for _ in 0..5 {
            bounded(Duration::from_secs(5), async {
                tokio::time::sleep(Duration::from_millis(4_900)).await;
                Ok(())
            })
            .await?;
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        Ok(())
    })
    .await;
    assert!(result.is_ok());
    assert!(started.elapsed() > RECOVERY_ITEM_TIMEOUT);
    assert!(started.elapsed() < PANEL_TIMEOUT);
    assert!(
        IDENTITY_TIMEOUT + RECOVERY_ITEM_TIMEOUT + RECOVERY_ROWS_BUDGET + PANEL_TIMEOUT
            < RECOVERY_TIMEOUT
    );
}

#[tokio::test]
async fn equal_timestamp_history_uses_snowflake_order_across_page_boundary() {
    let message = |id: u64| {
        json!({"id":id.to_string(),"timestamp":"2026-10-01T00:00:00.000Z",
        "author":{"username":"member","discriminator":"0"},
        "content":format!("message-{id}"),"attachments":[]})
    };
    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(
                200,
                json!((101..=200).rev().map(message).collect::<Vec<_>>()),
            ),
            ScriptedResponse::json(200, json!([message(100), message(99)])),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let snapshot = runtime(&mock).capture_history("600").await.ok().unwrap();
    assert_eq!(snapshot.message_count, 102);
    let lines: Vec<_> = snapshot.content.lines().collect();
    for (index, line) in lines.iter().enumerate() {
        assert!(line.ends_with(&format!("message-{}", 99 + index)));
    }
    assert_eq!(mock.requests().len(), 2);
    mock.shutdown().await;
}

#[tokio::test]
async fn unreadable_history_never_reposts_panels_or_controls() {
    let mut script = Vec::new();
    for channel in ["700", "700", "600", "600"] {
        script.extend([
            ScriptedResponse::json(200, json!({"id":channel,"guild_id":"100","type":0,"permission_overwrites":[]})),
            ScriptedResponse::json(200, json!({"user":{"id":"400"},"roles":[]})),
            ScriptedResponse::json(200, json!([{"id":"100","permissions":(Permissions::VIEW_CHANNEL|Permissions::SEND_MESSAGES).bits().to_string()}])),
        ]);
    }
    // A silent successful [] would be returned if the client queried history.
    let mock = MockRest::start(script, ScriptedResponse::json(200, json!([]))).await;
    let runtime = runtime(&mock);
    assert!(runtime.ensure_panel().await.is_err());
    assert!(runtime.ensure_panel().await.is_err());
    let mut ticket = recovery_ticket(1);
    ticket.channel_id = Some("600".into());
    assert!(runtime.ensure_controls(&ticket, "600").await.is_err());
    assert!(runtime.ensure_controls(&ticket, "600").await.is_err());
    assert!(mock
        .requests()
        .iter()
        .all(|r| r.method == "GET" && !r.path.contains("/messages")));
    mock.shutdown().await;
}
