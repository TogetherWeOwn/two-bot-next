//! Live destination moves apply to newly reserved tickets. A ticket whose
//! channel was created under previous destinations stays uncertain while the
//! live category or staff role differs: no state advance, no delete, no
//! controls. See docs/tickets.md.
#![cfg(test)]

use super::*;
use crate::discord_test_common::{MockRest, ScriptedResponse};
use serde_json::json;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
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

#[tokio::test]
async fn stored_rows_move_both_live_ticket_destinations_without_restart() {
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
    writer.publish(&SettingsSnapshot {
        revision: 1,
        rows: vec![
            SettingRow {
                guild_id: "100".to_owned(),
                key: "DISCORD_TICKET_CATEGORY_ID".to_owned(),
                value: json!("201"),
                version: 1,
            },
            SettingRow {
                guild_id: "100".to_owned(),
                key: "DISCORD_TICKET_STAFF_ROLE_ID".to_owned(),
                value: json!("301"),
                version: 1,
            },
        ],
    });
    runtime.refresh_config_with(Some(&live));
    let moved = runtime.config_for_test();
    assert_eq!(moved.category_id, "201");
    assert_eq!(moved.staff_role_id, "301");
    assert_eq!(moved.panel_channel_id, "700");
    assert_eq!(moved.cooldown_seconds, COOLDOWN_SECONDS);

    // A malformed stored staff row keeps the last good destinations.
    writer.publish(&SettingsSnapshot {
        revision: 2,
        rows: vec![
            SettingRow {
                guild_id: "100".to_owned(),
                key: "DISCORD_TICKET_CATEGORY_ID".to_owned(),
                value: json!("201"),
                version: 1,
            },
            SettingRow {
                guild_id: "100".to_owned(),
                key: "DISCORD_TICKET_STAFF_ROLE_ID".to_owned(),
                value: json!("not-a-snowflake"),
                version: 1,
            },
        ],
    });
    runtime.refresh_config_with(Some(&live));
    assert_eq!(runtime.config_for_test(), moved);
    mock.shutdown().await;
}

static NEXT_SCHEMA: AtomicU64 = AtomicU64::new(0);

struct TestDb {
    pool: PgPool,
    admin: PgPool,
    schema: String,
}

impl TestDb {
    async fn new() -> Self {
        match std::env::var("TICKET_TEST_DB_HOST").as_deref() {
            Ok("agent-testdb") | Err(_) => {}
            _ => {
                panic!("ticket acceptance permits only agent-testdb; never substitute credentials")
            }
        }
        let options = PgConnectOptions::new()
            .host("agent-testdb")
            .port(5432)
            .username("agent_test")
            .password("")
            .database("agent_test");
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect_with(options.clone())
            .await
            .expect("connect to authorized test container");
        let schema = format!(
            "ticket_move_{}_{}",
            std::process::id(),
            NEXT_SCHEMA.fetch_add(1, Ordering::Relaxed)
        );
        sqlx::QueryBuilder::new("CREATE SCHEMA ")
            .push(&schema)
            .build()
            .execute(&admin)
            .await
            .unwrap();
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect_with(options.options([("search_path", schema.clone())]))
            .await
            .unwrap();
        sqlx::raw_sql(include_str!("../../cutover/migrations/0210_tickets.sql"))
            .execute(&pool)
            .await
            .unwrap();
        Self {
            pool,
            admin,
            schema,
        }
    }

    async fn close(self) {
        self.pool.close().await;
        sqlx::QueryBuilder::new("DROP SCHEMA ")
            .push(&self.schema)
            .push(" CASCADE")
            .build()
            .execute(&self.admin)
            .await
            .unwrap();
        self.admin.close().await;
    }
}

/// Channel evidence as created under the previous destinations: category 200,
/// staff role 300, exact reservation topic, four exact overwrites.
fn created_channel(channel: &str, topic: &str) -> Value {
    let common = (Permissions::VIEW_CHANNEL
        | Permissions::SEND_MESSAGES
        | Permissions::READ_MESSAGE_HISTORY)
        .bits();
    json!({"id":channel,"guild_id":"100","type":0,"parent_id":"200","topic":topic,
    "permission_overwrites":[
        {"id":"100","type":0,"allow":"0","deny":Permissions::VIEW_CHANNEL.bits().to_string()},
        {"id":"400","type":1,"allow":(common | Permissions::MANAGE_CHANNELS.bits()).to_string(),"deny":"0"},
        {"id":"500","type":1,"allow":common.to_string(),"deny":"0"},
        {"id":"300","type":0,"allow":common.to_string(),"deny":"0"},
    ]})
}

#[derive(Clone, Copy)]
enum Setup {
    Creating,
    CleanupPending,
    Open,
}

async fn check_uncertain(
    id: &str,
    channel: &str,
    category: &str,
    staff: &str,
    setup: Setup,
) -> Vec<String> {
    let mut failures = Vec::new();
    let db = TestDb::new().await;
    let doc = created_channel(channel, &format!("two-ticket:{id}"));
    let path = format!("/api/v10/channels/{channel}");
    let mock = MockRest::with_responder(move |request| {
        if request.method == "GET" && request.path == path {
            ScriptedResponse::json(200, doc.clone())
        } else {
            ScriptedResponse::status(500)
        }
    })
    .await;
    let executor =
        ActionExecutor::with_proxy("ticket-test-token".into(), Some(mock.origin())).unwrap();
    // Post-move steady state: the live destinations differ from the channel's
    // creation destinations. (The global settings poller is absent in tests,
    // so the runtime is constructed at the moved snapshot directly; the
    // refresh test above proves stored rows land there live.)
    let config = TicketConfig {
        guild_id: "100".into(),
        category_id: category.into(),
        panel_channel_id: "700".into(),
        staff_role_id: staff.into(),
        cooldown_seconds: COOLDOWN_SECONDS,
    };
    let deployment = HashMap::from([
        ("DISCORD_TICKET_CATEGORY_ID".to_owned(), category.to_owned()),
        (
            "DISCORD_TICKET_PANEL_CHANNEL_ID".to_owned(),
            "700".to_owned(),
        ),
        ("DISCORD_TICKET_STAFF_ROLE_ID".to_owned(), staff.to_owned()),
    ]);
    let tickets = TicketRuntime::new(db.pool.clone(), executor, config, deployment).unwrap();
    tickets.set_bot_id(400);
    match tickets.store.reserve(id, "500", 0, 0).await {
        Ok(OpenResult::Created(_)) => {}
        _ => failures.push(format!("{id}: expected a fresh reservation")),
    }
    tickets.store.record_channel(id, channel).await.unwrap();
    match setup {
        Setup::Creating => {}
        Setup::CleanupPending => {
            sqlx::query("UPDATE tickets SET status='cleanup_pending' WHERE id=$1")
                .bind(id)
                .execute(&db.pool)
                .await
                .unwrap();
        }
        Setup::Open => {
            tickets.store.activate(id, channel).await.unwrap();
        }
    }
    let ticket = tickets.store.get(id).await.unwrap().unwrap();
    let result = tickets.recover_ticket(&ticket).await;
    let after = tickets.store.get(id).await.unwrap().unwrap();
    // The mock serves a complete 200 channel document, so any error is the
    // proof refusal itself (receipt errors convert to Failure::Rest via `?`).
    // Pin refusal + retention + no mutation, not the incidental variant.
    if result.is_ok() {
        failures.push(format!(
            "{id}: moved destinations must stay uncertain without success"
        ));
    }
    let expected = match setup {
        Setup::Creating => TicketStatus::Creating,
        Setup::CleanupPending => TicketStatus::CleanupPending,
        Setup::Open => TicketStatus::Open,
    };
    if after.status != expected {
        failures.push(format!("{id}: uncertain proof must not advance state"));
    }
    let requests = mock.requests();
    if requests.len() != 1 || requests.iter().any(|r| r.method != "GET") {
        failures.push(format!(
            "{id}: uncertain recovery must be one read, saw {} requests",
            requests.len()
        ));
    }
    mock.shutdown().await;
    db.close().await;
    failures
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI service container"]
async fn moved_live_destinations_keep_in_flight_tickets_uncertain() {
    let mut failures = Vec::new();
    for (id, channel, category, staff, setup) in [
        ("move-creating", "611", "201", "301", Setup::Creating),
        ("move-creating-staff", "612", "200", "301", Setup::Creating),
        ("move-rollback", "613", "201", "301", Setup::CleanupPending),
        ("move-reattach", "614", "201", "301", Setup::Open),
    ] {
        failures.extend(check_uncertain(id, channel, category, staff, setup).await);
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
