//! Mock-only proofs; lazy testdb pool never connects in these tests.
//! Timer proofs use Tokio's paused clock, not elapsed wall time.
use super::*;
use crate::command_runtime::CommandRuntime;
use crate::discord_test_common::{MockRest, ScriptedResponse};
use serde_json::json;
use sqlx::postgres::PgPoolOptions;
use std::sync::Arc;
use twilight_model::application::interaction::Interaction;
use twilight_model::gateway::event::Event;
use twilight_model::gateway::payload::incoming::InteractionCreate;
use two_bot_core::tickets::TICKET_OPEN_ID;

fn runtime(mock: &MockRest) -> TicketRuntime {
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://agent_test@agent-testdb:5432/agent_test")
        .unwrap();
    let rest = ActionExecutor::with_proxy("ticket-test-token".into(), Some(mock.origin())).unwrap();
    let runtime = TicketRuntime::new(
        pool,
        rest,
        TicketConfig {
            guild_id: "100".into(),
            category_id: "200".into(),
            panel_channel_id: "700".into(),
            staff_role_id: "300".into(),
            cooldown_seconds: COOLDOWN_SECONDS,
        },
        std::collections::HashMap::new(),
    )
    .unwrap();
    runtime.set_bot_id(400);
    runtime
}

fn message(id: u64) -> Value {
    json!({
        "id":id.to_string(), "timestamp":two_bot_core::funnel::format_iso_millis(id as i64),
        "author":{"id":"500","username":"member","discriminator":"0"},
        "content":format!("message-{id}"), "attachments":[{"url":format!("https://example.test/{id}")}],
    })
}

use std::collections::HashMap;
use two_bot_core::settings::{live_channel, SettingRow, SettingsSnapshot};

fn deployment_vars() -> HashMap<String, String> {
    HashMap::from([
        ("DISCORD_TICKET_CATEGORY_ID".to_owned(), "200".to_owned()),
        (
            "DISCORD_TICKET_PANEL_CHANNEL_ID".to_owned(),
            "700".to_owned(),
        ),
        ("DISCORD_TICKET_STAFF_ROLE_ID".to_owned(), "300".to_owned()),
    ])
}

#[test]
fn ticket_destinations_resolve_and_conflicts_refuse() {
    let vars = deployment_vars();
    let config = TicketConfig::from_vars(&vars, 100, COOLDOWN_SECONDS).unwrap();
    assert_eq!(config.category_id, "200");
    assert_eq!(config.panel_channel_id, "700");
    assert_eq!(config.staff_role_id, "300");
    assert_eq!(config.cooldown_seconds, COOLDOWN_SECONDS);

    // The staff role must not be the guild id.
    let mut staff_is_guild = vars.clone();
    staff_is_guild.insert("DISCORD_TICKET_STAFF_ROLE_ID".to_owned(), "100".to_owned());
    assert!(TicketConfig::from_vars(&staff_is_guild, 100, COOLDOWN_SECONDS).is_none());

    // Category and panel must differ.
    let mut panel_is_category = vars.clone();
    panel_is_category.insert(
        "DISCORD_TICKET_PANEL_CHANNEL_ID".to_owned(),
        "200".to_owned(),
    );
    assert!(TicketConfig::from_vars(&panel_is_category, 100, COOLDOWN_SECONDS).is_none());

    // Malformed and missing ids refuse without a config.
    for bad in ["0", "abc", " 200", "0200"] {
        let mut malformed = vars.clone();
        malformed.insert("DISCORD_TICKET_CATEGORY_ID".to_owned(), bad.to_owned());
        assert!(
            TicketConfig::from_vars(&malformed, 100, COOLDOWN_SECONDS).is_none(),
            "{bad:?}"
        );
    }
    let mut missing = vars.clone();
    missing.remove("DISCORD_TICKET_STAFF_ROLE_ID");
    assert!(TicketConfig::from_vars(&missing, 100, COOLDOWN_SECONDS).is_none());
}

#[tokio::test]
async fn stored_destination_rows_move_the_live_ticket_config_without_restart() {
    let mock = MockRest::start(Vec::new(), ScriptedResponse::status(500)).await;
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://agent_test@agent-testdb:5432/agent_test")
        .unwrap();
    let executor =
        ActionExecutor::with_proxy("ticket-test-token".into(), Some(mock.origin())).unwrap();
    let runtime = TicketRuntime::new(
        pool,
        executor,
        TicketConfig::from_vars(&deployment_vars(), 100, COOLDOWN_SECONDS).unwrap(),
        deployment_vars(),
    )
    .unwrap();

    let (mut writer, live) = live_channel();
    // A stored category row wins over the boot deployment value on the next
    // refresh; panel, staff and the cold cooldown are untouched.
    writer.publish(&SettingsSnapshot {
        revision: 1,
        rows: vec![SettingRow {
            guild_id: "100".to_owned(),
            key: "DISCORD_TICKET_CATEGORY_ID".to_owned(),
            value: json!("201"),
            version: 1,
        }],
    });
    runtime.refresh_config_with(Some(&live));
    let moved = runtime.config_for_test();
    assert_eq!(moved.category_id, "201");
    assert_eq!(moved.panel_channel_id, "700");
    assert_eq!(moved.staff_role_id, "300");
    assert_eq!(moved.cooldown_seconds, COOLDOWN_SECONDS);

    // A malformed stored row keeps the last good config.
    writer.publish(&SettingsSnapshot {
        revision: 2,
        rows: vec![SettingRow {
            guild_id: "100".to_owned(),
            key: "DISCORD_TICKET_CATEGORY_ID".to_owned(),
            value: json!("not-a-snowflake"),
            version: 1,
        }],
    });
    runtime.refresh_config_with(Some(&live));
    assert_eq!(runtime.config_for_test(), moved);

    // Deleting the row hands the key back to the boot deployment value.
    writer.publish(&SettingsSnapshot {
        revision: 3,
        rows: vec![],
    });
    runtime.refresh_config_with(Some(&live));
    assert_eq!(
        runtime.config_for_test(),
        TicketConfig::from_vars(&deployment_vars(), 100, COOLDOWN_SECONDS).unwrap()
    );
    mock.shutdown().await;
}

#[test]
fn ordinary_messages_without_optional_components_do_not_block_panel_ensure() {
    let messages = panels(vec![
        json!({"author":{"id":"500"},"content":"ordinary message"}),
    ])
    .ok()
    .expect("ordinary Discord message");
    assert!(panel_needed("400", &messages));
    let isolated = panels(vec![json!({"author":{"id":"500"},"components":{}})])
        .ok()
        .expect("malformed panel message is isolated, batch stays Ok");
    assert!(isolated.is_empty());
}

#[test]
fn malformed_panel_rows_are_counted_without_echoing_content() {
    let before = panel_isolated_count_for_test();
    let isolated = panels(vec![
        json!({"author": {"id": "500"}, "components": {}}),
        json!({"author": {"id": "400"}, "components": []}),
    ])
    .ok()
    .expect("one malformed row is isolated");
    assert_eq!(isolated.len(), 1);
    assert_eq!(isolated[0].author_id, "400");
    assert!(panel_isolated_count_for_test() > before);
}

#[test]
fn open_retry_backoff_doubles_to_the_hourly_cap() {
    assert_eq!(open_retry_backoff(0), Duration::from_secs(300));
    assert_eq!(open_retry_backoff(1), Duration::from_secs(600));
    assert_eq!(open_retry_backoff(2), Duration::from_secs(1200));
    assert_eq!(open_retry_backoff(3), Duration::from_secs(2400));
    assert_eq!(open_retry_backoff(4), Duration::from_secs(3600));
    assert_eq!(open_retry_backoff(u32::MAX), Duration::from_secs(3600));
    // The first step is exactly one recovery cadence: an immediate
    // recover/Ready burst at the same clock reading replays nothing, and the
    // next periodic pass is already eligible.
    assert_eq!(open_retry_backoff(0), RECOVERY_CADENCE);
}

#[test]
fn open_retry_bounds_match_the_acceptance_envelope() {
    // Pinned against the protected retry acceptance envelope: retention is
    // finite but never discards every failure, and one pass never spends
    // more than the per-pass work cap on retries.
    assert_eq!(OPEN_RETRY_RETAINED_CAP, 128);
    assert_eq!(OPEN_RETRY_PER_PASS_CAP, 64);
    // Const context: clippy `assertions_on_constants` rejects this as a
    // runtime assert, and a cap edited out of order fails the build here.
    const _: () = {
        assert!(OPEN_RETRY_PER_PASS_CAP <= OPEN_RETRY_RETAINED_CAP);
    };
}

#[test]
fn open_retry_reasons_are_bounded_machine_tokens() {
    use two_bot_cutover::tickets::StoreError;
    let failures = [
        Failure::Store(StoreError::NotFound),
        Failure::Store(StoreError::InvalidRow("status")),
        Failure::Store(StoreError::Domain(
            two_bot_core::tickets::TicketError::AlreadyClaimed,
        )),
        Failure::Rest(DiscordError::Timeout),
        Failure::Rest(DiscordError::RateLimited),
        Failure::Rest(DiscordError::Rejected("rejected".into())),
        Failure::Rest(DiscordError::Unavailable("unavailable".into())),
        Failure::InvalidEvidence,
        Failure::Timeout,
        Failure::LaneBusy,
        Failure::NoTicket,
        Failure::TicketClaimedBy("1".into()),
        Failure::TicketNotOpen,
    ];
    for failure in failures {
        let reason = open_retry_reason(&failure);
        assert!(
            !reason.is_empty()
                && reason.len() <= 64
                && reason
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
            "bounded machine token: {reason:?}"
        );
    }
    // Database failures reuse the shared `db_cause` vocabulary, whose own
    // token shape is pinned in `two_bot_core::error_chain` tests.
}

#[tokio::test]
async fn ticket_history_fetches_all_pages_and_keeps_attachments_and_total_count() {
    let first: Vec<_> = (901..=1000).rev().map(message).collect();
    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(200, json!(first)),
            ScriptedResponse::json(200, json!([message(900), message(899)])),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let (snapshot, truncated) = runtime(&mock)
        .capture_history("600")
        .await
        .ok()
        .expect("complete history");
    assert!(!truncated);
    assert_eq!(snapshot.message_count, 102);
    assert!(snapshot
        .content
        .starts_with("[1970-01-01T00:00:00.899Z] member: message-899 https://example.test/899"));
    assert!(snapshot
        .content
        .ends_with("message-1000 https://example.test/1000"));
    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests
        .iter()
        .all(|request| request.method == "GET" && request.path.contains("limit=100")));
    assert!(requests[1].path.contains("before=901"));
    mock.shutdown().await;
}

#[tokio::test]
async fn truncated_transcript_still_fetches_later_pages_and_counts_every_message() {
    let mut first: Vec<_> = (901..=1000).rev().map(message).collect();
    first[0]["content"] = json!("😀".repeat(110_000));
    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(200, json!(first)),
            ScriptedResponse::json(200, json!([message(900)])),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let (snapshot, truncated) = runtime(&mock)
        .capture_history("600")
        .await
        .ok()
        .expect("complete capped history");
    assert!(truncated);
    assert_eq!(snapshot.message_count, 101);
    let retained = snapshot
        .content
        .strip_suffix("\n[transcript truncated]")
        .expect("truncation marker");
    assert!(retained.encode_utf16().count() <= MAX_TRANSCRIPT_UTF16_UNITS);
    assert_eq!(mock.requests().len(), 2);
    mock.shutdown().await;
}

#[tokio::test]
async fn partial_duplicate_malformed_and_nonprogressing_history_never_complete() {
    let first: Vec<_> = (901..=1000).rev().map(message).collect();
    for second in [
        ScriptedResponse::status(500),
        ScriptedResponse::json(200, json!([message(999)])),
        ScriptedResponse::json(200, json!([message(900), message(900)])),
        ScriptedResponse::json(
            200,
            json!([{"id":"900","content":"missing attachments and timestamp"}]),
        ),
    ] {
        let mock = MockRest::start(
            vec![ScriptedResponse::json(200, json!(first)), second],
            ScriptedResponse::status(500),
        )
        .await;
        assert!(runtime(&mock).capture_history("600").await.is_err());
        assert_eq!(mock.requests().len(), 2);
        assert!(mock
            .requests()
            .iter()
            .all(|request| request.method == "GET"));
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn exact_full_page_requires_a_final_empty_page() {
    let first: Vec<_> = (901..=1000).rev().map(message).collect();
    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(200, json!(first)),
            ScriptedResponse::json(200, json!([])),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let (snapshot, truncated) = runtime(&mock)
        .capture_history("600")
        .await
        .ok()
        .expect("final empty page");
    assert!(!truncated);
    assert_eq!(snapshot.message_count, 100);
    assert_eq!(mock.requests().len(), 2);
    mock.shutdown().await;
}

#[tokio::test]
async fn marker_text_in_complete_history_reports_not_truncated() {
    let mut newest = message(2);
    newest["content"] = json!("ok\n[transcript truncated]");
    newest["attachments"] = json!([]);
    let mock = MockRest::start(
        vec![ScriptedResponse::json(200, json!([newest, message(1)]))],
        ScriptedResponse::status(500),
    )
    .await;
    let (snapshot, truncated) = runtime(&mock)
        .capture_history("600")
        .await
        .ok()
        .expect("complete history");
    assert!(
        !truncated,
        "user-controlled marker text must not report truncation"
    );
    assert!(snapshot.content.ends_with("\n[transcript truncated]"));
    assert_eq!(snapshot.message_count, 2);
    mock.shutdown().await;
}

fn panel_channel() -> Value {
    let allow = (Permissions::VIEW_CHANNEL | Permissions::READ_MESSAGE_HISTORY).bits();
    json!({"id":"700","guild_id":"100","type":0,"permission_overwrites":[
        {"id":"400","type":1,"allow":allow.to_string(),"deny":"0"}
    ]})
}
fn panel(author: &str) -> Value {
    json!({"author":{"id":author},"components":[{"type":1,"components":[{"type":2,"custom_id":TICKET_OPEN_ID}]}]})
}

#[tokio::test]
async fn panel_ensure_trusts_only_this_bot_and_is_idempotent() {
    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(200, panel_channel()),
            ScriptedResponse::json(200, json!([panel("500")])),
            ScriptedResponse::json(200, json!({"id":"800"})),
            ScriptedResponse::json(200, panel_channel()),
            ScriptedResponse::json(200, json!([panel("400")])),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let runtime = runtime(&mock);
    assert!(runtime.ensure_panel().await.is_ok());
    assert!(runtime.ensure_panel().await.is_ok());
    let requests = mock.requests();
    assert_eq!(requests.len(), 5);
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.method == "POST")
            .count(),
        1
    );
    assert!(requests[1].path.contains("limit=50"));
    mock.shutdown().await;
}

#[tokio::test]
async fn panel_ensure_refuses_foreign_guild_and_access_failures_without_posting() {
    for response in [
        ScriptedResponse::json(200, json!({"id":"700","guild_id":"999","type":0})),
        ScriptedResponse::json(200, json!({"id":"700","guild_id":"100","type":2})),
        ScriptedResponse::json(403, json!({"code":50013})),
    ] {
        let mock = MockRest::start(vec![], response).await;
        assert!(runtime(&mock).ensure_panel().await.is_err());
        assert_eq!(mock.requests().len(), 1);
        assert_eq!(mock.requests()[0].method, "GET");
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn history_requires_explicit_bot_view_and_read_overwrite() {
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let runtime = runtime(&mock);
    let permissions = (Permissions::VIEW_CHANNEL | Permissions::READ_MESSAGE_HISTORY).bits();
    let doc = json!({"permission_overwrites":[{"id":"400","type":1,"allow":permissions.to_string(),"deny":"0"}]});
    assert!(runtime.require_history_access(&doc).is_ok());
    for doc in [
        json!({"permission_overwrites":[]}),
        json!({"permission_overwrites":[{"id":"400","type":0,"allow":permissions.to_string(),"deny":"0"}]}),
        json!({"permission_overwrites":[{"id":"400","type":1,"allow":permissions.to_string(),"deny":Permissions::READ_MESSAGE_HISTORY.bits().to_string()}]}),
    ] {
        assert!(runtime.require_history_access(&doc).is_err());
    }
    assert!(mock.requests().is_empty());
    mock.shutdown().await;
}

#[tokio::test]
async fn component_admission_ignores_generic_ticket_tasks() {
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let tickets = runtime(&mock);
    // A generic ticket task sharing the scope (e.g. RESUMED readiness) must
    // not consume component budget: all 16 components still admit with one
    // generic task in flight, and only the 17th component refuses.
    tickets.spawn(std::future::pending());
    for member in 0..(MAX_TICKET_COMPONENT_IN_FLIGHT as u64) {
        assert!(
            tickets.try_spawn_component(Some(member), |permit| async move {
                let _permit = permit;
                std::future::pending::<()>().await;
            }),
            "component {member} refused while budget remained"
        );
    }
    assert!(
        !tickets.try_spawn_component(Some(u64::MAX), |permit| async move {
            let _permit = permit;
        })
    );
    assert!(mock.requests().is_empty());
    mock.shutdown().await;
}

/// Wire-format ticket open for saturation tests. Distinctive member and
/// interaction ids keep log assertions exact under parallel suites.
fn saturation_open(member: u64, interaction_id: u64) -> Interaction {
    let roles: Vec<&str> = vec![];
    serde_json::from_value(json!({
        "application_id": "400",
        "authorizing_integration_owners": {"0": "100"},
        "id": interaction_id.to_string(),
        "token": format!("ticket-saturation-{member}-{interaction_id}"),
        "type": 3,
        "version": 1,
        "guild_id": "100",
        "channel": {"id": "600", "type": 0, "name": "ticket-member", "permissions": "0"},
        "data": {"custom_id": TICKET_OPEN_ID, "component_type": 2},
        "member": {
            "roles": roles,
            "permissions": "0",
            "deaf": false, "mute": false, "flags": 0,
            "user": {"id": member.to_string(), "username": "Member", "discriminator": "0", "avatar": null}
        }
    }))
    .expect("wire-format ticket interaction")
}

/// Command runtime whose ticket budget is already full: 16 admitted
/// components held pending, so every dispatch through the gateway path
/// refuses synchronously without spawning and without defer timing.
fn saturated_command_runtime(mock: &MockRest) -> Arc<CommandRuntime> {
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://agent_test@agent-testdb:5432/agent_test")
        .unwrap();
    let executor =
        ActionExecutor::with_proxy("ticket-test-token".into(), Some(mock.origin())).unwrap();
    let tickets = Arc::new(
        TicketRuntime::new(
            pool.clone(),
            executor.clone(),
            TicketConfig {
                guild_id: "100".into(),
                category_id: "200".into(),
                panel_channel_id: "700".into(),
                staff_role_id: "300".into(),
                cooldown_seconds: COOLDOWN_SECONDS,
            },
            std::collections::HashMap::new(),
        )
        .unwrap(),
    );
    tickets.set_bot_id(400);
    for member in 0..(MAX_TICKET_COMPONENT_IN_FLIGHT as u64) {
        assert!(
            tickets.try_spawn_component(Some(member), |permit| async move {
                let _permit = permit;
                std::future::pending::<()>().await;
            }),
            "setup must fill the component budget"
        );
    }
    CommandRuntime::with_tickets(pool, executor, tickets)
}

/// Read the ticket component drops series from the process-global metrics
/// exposition. Lower-bound asserts only: parallel suites share the registry.
fn global_drops() -> u64 {
    two_bot_core::metrics::global()
        .render(None)
        .lines()
        .filter(|line| line.starts_with("two_bot_ticket_component_drops_total "))
        .map(|line| {
            line.rsplit_once(' ')
                .and_then(|(_, value)| value.parse::<u64>().ok())
                .unwrap_or_else(|| panic!("bad sample: {line}"))
        })
        .sum()
}

#[tokio::test]
async fn component_refusal_records_saturation_evidence_through_dispatch() {
    crate::tracing_capture::global();
    let mock = MockRest::start(vec![], ScriptedResponse::status(204)).await;
    let runtime = saturated_command_runtime(&mock);
    let _guard = runtime.dispatch_guard();

    // Eight refusals from distinct members through the real gateway path.
    // The counter check is a lower bound because parallel suites share the
    // global registry; the log check below is exact. Removing the dispatch
    // record call fails this test: our refusals add nothing to the counter
    // and our ids never appear in the capture.
    let drops_before = global_drops();
    for index in 0..8u64 {
        let event = Event::InteractionCreate(Box::new(InteractionCreate(saturation_open(
            80_000 + index,
            940_000 + index,
        ))));
        assert!(
            !runtime.dispatch(&event),
            "dispatch {index} must refuse on the saturated budget"
        );
    }
    let drops_after = global_drops();
    assert!(
        drops_after >= drops_before + 8,
        "every dispatch refusal increments the drops counter ({drops_before} -> {drops_after})"
    );

    // The sampled warn fires through dispatch too: the first refusal on this
    // runtime logs its interaction id with the saturation code. Scoping by
    // our distinctive ids keeps this exact under parallel suites.
    let logged = crate::tracing_capture::global().text();
    assert!(
        logged
            .lines()
            .any(|line| line.contains("ticket_component_admission_saturated")
                && (940_000..940_008).any(|id| line.contains(&id.to_string()))),
        "a dispatch refusal must log the saturation code with its interaction id"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn component_saturation_warn_samples_to_one_line_per_window() {
    let capture = crate::tracing_capture::global();
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let tickets = runtime(&mock);

    // Three refusals in one window on a fresh runtime: the counter moves
    // three times but the log samples to exactly one line. Distinctive
    // interaction ids keep the count exact under parallel suites.
    let drops_before = global_drops();
    tickets.record_component_saturation(941_001);
    tickets.record_component_saturation(941_002);
    tickets.record_component_saturation(941_003);
    assert!(
        global_drops() >= drops_before + 3,
        "every refusal increments the drops counter even when the log samples it out"
    );
    let logged = capture.text();
    let lines: Vec<&str> = logged
        .lines()
        .filter(|line| {
            line.contains("ticket_component_admission_saturated")
                && (line.contains("941001") || line.contains("941002") || line.contains("941003"))
        })
        .collect();
    assert_eq!(
        lines.len(),
        1,
        "one sampled line per window, got: {lines:?}"
    );
    assert!(
        lines[0].contains("two_bot_ticket_component_drops_total"),
        "the sampled line points on-call at the counter series: {}",
        lines[0]
    );
    mock.shutdown().await;
}
