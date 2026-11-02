//! Protected acceptance: the real runtime, SQL store and REST executor must not
//! activate or post controls on unproven private-channel creation evidence.
//! Only loopback Discord and TestDatabase's guarded disposable service are used.
#![cfg(test)]

use super::*;
use crate::discord_test_common::{MockRest, ScriptedResponse};
use serde_json::json;
use two_bot_testsupport::TestDatabase;

#[derive(Clone, Copy, Debug)]
enum Evidence {
    Valid,
    IdOnly,
    WrongGuild,
    WrongType,
    WrongParent,
    WrongTopic,
    MissingTopic,
    MissingOverwrites,
    Public,
    Duplicate,
    Contradictory,
    ExtraViewer,
    MalformedJson,
}

fn receipt(topic: &str, evidence: Evidence) -> Value {
    let mut body = json!({
        "id":"600", "guild_id":"100", "type":0, "parent_id":"200", "topic":topic,
        "permission_overwrites":[
            {"id":"100","type":0,"allow":"0","deny":"1024"},
            {"id":"400","type":1,"allow":"68624","deny":"0"},
            {"id":"500","type":1,"allow":"68608","deny":"0"},
            {"id":"300","type":0,"allow":"68608","deny":"0"}
        ],
        "name":"receipt-private-channel-sentinel"
    });
    match evidence {
        Evidence::Valid | Evidence::MalformedJson => {}
        Evidence::IdOnly => body = json!({"id":"600"}),
        Evidence::WrongGuild => body["guild_id"] = json!("101"),
        Evidence::WrongType => body["type"] = json!(4),
        Evidence::WrongParent => body["parent_id"] = json!("201"),
        Evidence::WrongTopic => body["topic"] = json!("two-ticket:another-reservation"),
        Evidence::MissingTopic => {
            body.as_object_mut().unwrap().remove("topic");
        }
        Evidence::MissingOverwrites => {
            body.as_object_mut()
                .unwrap()
                .remove("permission_overwrites");
        }
        Evidence::Public => body["permission_overwrites"][0]["deny"] = json!("0"),
        Evidence::Duplicate => {
            let row = body["permission_overwrites"][2].clone();
            body["permission_overwrites"]
                .as_array_mut()
                .unwrap()
                .push(row);
        }
        Evidence::Contradictory => body["permission_overwrites"][2]["deny"] = json!("1024"),
        Evidence::ExtraViewer => body["permission_overwrites"]
            .as_array_mut()
            .unwrap()
            .push(json!({"id":"501","type":1,"allow":"1024","deny":"0"})),
    }
    body
}

fn response(topic: &str, evidence: Evidence, status: u16) -> ScriptedResponse {
    if matches!(evidence, Evidence::MalformedJson) {
        let mut response = ScriptedResponse::status(status);
        response.body = b"{\"id\":\"600\",receipt-private-body-sentinel".to_vec();
        response
    } else {
        ScriptedResponse::json(status, receipt(topic, evidence))
    }
}

async fn fixture() -> Option<TestDatabase> {
    let Ok(url) = std::env::var("TWO_TEST_DATABASE_URL") else {
        assert!(
            std::env::var("GITHUB_ACTIONS").is_err(),
            "CI requires guarded test DB"
        );
        eprintln!("SKIP ticket receipt acceptance: TWO_TEST_DATABASE_URL is not set");
        return None;
    };
    Some(
        TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("create guarded disposable test database"),
    )
}

fn runtime(pool: PgPool, mock: &MockRest) -> Arc<TicketRuntime> {
    crate::gateway::ensure_crypto_provider();
    let executor = ActionExecutor::with_proxy("receipt-fixture-token".into(), Some(mock.origin()))
        .expect("loopback executor");
    let runtime = Arc::new(
        TicketRuntime::new(
            pool,
            executor,
            TicketConfig {
                guild_id: "100".into(),
                category_id: "200".into(),
                panel_channel_id: "700".into(),
                staff_role_id: "300".into(),
                cooldown_seconds: 0,
            },
            HashMap::new(),
        )
        .expect("ticket runtime"),
    );
    runtime.set_bot_id(400);
    runtime
}

fn open_button() -> Interaction {
    serde_json::from_value(json!({
        "application_id":"400", "authorizing_integration_owners":{"0":"100"},
        "id":"800", "token":"receipt-interaction-token-sentinel", "type":3, "version":1,
        "guild_id":"100", "channel":{"id":"700","type":0,"name":"tickets","permissions":"0"},
        "data":{"custom_id":"two:tickets:open","component_type":2},
        "member":{"roles":[],"permissions":"0","deaf":false,"mute":false,"flags":0,
            "user":{"id":"500","username":"receipt-member-sentinel","discriminator":"0","avatar":null}}
    })).expect("wire ticket button")
}

fn another_open_button() -> Interaction {
    let mut button = open_button();
    button.id = "801".parse().expect("distinct interaction snowflake");
    button
}

async fn mock(evidence: Evidence) -> (MockRest, Arc<TaskMutex<String>>) {
    let topic = Arc::new(TaskMutex::new(String::new()));
    let captured = Arc::clone(&topic);
    let mock = MockRest::with_responder(move |request| {
        match (request.method.as_str(), request.path.as_str()) {
            ("GET", "/api/v10/channels/200") => {
                ScriptedResponse::json(200, json!({"id":"200","guild_id":"100","type":4}))
            }
            ("POST", "/api/v10/guilds/100/channels") => {
                let body: Value =
                    serde_json::from_slice(&request.body).expect("create request JSON");
                let requested_topic = body["topic"]
                    .as_str()
                    .expect("reservation topic")
                    .to_owned();
                *captured.lock().unwrap() = requested_topic.clone();
                response(&requested_topic, evidence, 201)
            }
            ("GET", "/api/v10/channels/600") => response(&captured.lock().unwrap(), evidence, 200),
            ("POST", "/api/v10/channels/600/messages") => {
                ScriptedResponse::json(200, json!({"id":"900"}))
            }
            ("POST", "/api/v10/interactions/800/receipt-interaction-token-sentinel/callback") => {
                ScriptedResponse::status(204)
            }
            (
                "PATCH",
                "/api/v10/webhooks/400/receipt-interaction-token-sentinel/messages/@original",
            ) => ScriptedResponse::json(200, json!({})),
            _ => ScriptedResponse::status(500),
        }
    })
    .await;
    (mock, topic)
}

async fn rows(pool: &PgPool) -> Vec<(String, String, Option<String>)> {
    sqlx::query_as("SELECT id, status, channel_id FROM tickets ORDER BY id")
        .fetch_all(pool)
        .await
        .expect("read owned fixture rows")
}

fn safe_uncertain_reply(reply: &str) -> bool {
    let lower = reply.to_lowercase();
    reply.len() <= 300
        && !["ticket is ready", "ticket's ready", "ticket ready"]
            .iter()
            .any(|phrase| lower.contains(phrase))
        && !reply.contains("<#600>")
        && !reply.contains("sentinel")
        && ["recover", "staff", "try again", "retry", "help"]
            .iter()
            .any(|word| lower.contains(word))
}

async fn trace_activation(pool: &PgPool) {
    // Persist observations in the owned fixture so an activate-then-revert cannot
    // hide a transiently committed open state from the final row assertion.
    sqlx::raw_sql(
        "CREATE TABLE receipt_status_events (status text NOT NULL);
         CREATE FUNCTION trace_receipt_status() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN INSERT INTO receipt_status_events VALUES (NEW.status); RETURN NEW; END $$;
         CREATE TRIGGER receipt_status_trace AFTER INSERT OR UPDATE ON tickets
         FOR EACH ROW EXECUTE FUNCTION trace_receipt_status();",
    )
    .execute(pool)
    .await
    .expect("install fixture-only transition observer");
}

async fn invalid_flow(evidence: Evidence) -> Vec<String> {
    let Some(db) = fixture().await else {
        return Vec::new();
    };
    trace_activation(db.pool()).await;
    let (mock, topic) = mock(evidence).await;
    let tickets = runtime(db.pool().clone(), &mock);
    let mut failures = Vec::new();
    let result = tickets.execute(&open_button(), TicketAction::Open).await;
    match result {
        Ok(_) => failures.push(format!("{evidence:?}: invalid create reported success")),
        Err(error) => {
            if !safe_uncertain_reply(&error.reply()) {
                failures.push(format!(
                    "{evidence:?}: member copy not short/truthful/actionable"
                ));
            }
        }
    }
    let activations: i64 =
        sqlx::query_scalar("SELECT count(*) FROM receipt_status_events WHERE status='open'")
            .fetch_one(db.pool())
            .await
            .expect("read committed transitions");
    if activations != 0 {
        failures.push(format!(
            "{evidence:?}: unproven receipt committed an activation"
        ));
    }
    let before = rows(db.pool()).await;
    if before.len() != 1 || before[0].1 != "creating" {
        failures.push(format!(
            "{evidence:?}: creating reservation not retained: {before:?}"
        ));
    }
    if before.len() == 1 && *topic.lock().unwrap() != format!("two-ticket:{}", before[0].0) {
        failures.push(format!(
            "{evidence:?}: create not tied to durable reservation"
        ));
    }
    // Repeated member action must reuse the uncertainty, not create under a new reservation.
    let _ = tickets
        .execute(&another_open_button(), TicketAction::Open)
        .await;
    if rows(db.pool()).await != before {
        failures.push(format!(
            "{evidence:?}: repeat open changed reservation/state"
        ));
    }
    let requests = mock.requests();
    if requests
        .iter()
        .filter(|r| r.method == "POST" && r.path.ends_with("/guilds/100/channels"))
        .count()
        != 1
    {
        failures.push(format!("{evidence:?}: create retried"));
    }
    if requests
        .iter()
        .any(|r| r.method == "POST" && r.path.ends_with("/channels/600/messages"))
    {
        failures.push(format!(
            "{evidence:?}: opener controls posted on unproven channel"
        ));
    }
    if requests
        .iter()
        .any(|r| r.method == "DELETE" || r.method == "PUT")
    {
        failures.push(format!(
            "{evidence:?}: attempted unproven delete/permission repair"
        ));
    }
    if requests
        .iter()
        .filter(|r| r.method == "GET" && r.path.ends_with("/channels/600"))
        .count()
        > 1
    {
        failures.push(format!("{evidence:?}: corroborating fetch unbounded"));
    }
    mock.shutdown().await;
    drop(tickets);
    db.close().await.expect("teardown owned test database");
    failures
}

#[tokio::test]
async fn unproven_identity_never_activates_or_posts_and_reuses_creating_reservation() {
    let mut failures = Vec::new();
    for evidence in [
        Evidence::IdOnly,
        Evidence::WrongGuild,
        Evidence::WrongType,
        Evidence::WrongParent,
        Evidence::WrongTopic,
        Evidence::MissingTopic,
    ] {
        failures.extend(invalid_flow(evidence).await);
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[tokio::test]
async fn unproven_privacy_never_activates_or_posts_and_reuses_creating_reservation() {
    let mut failures = Vec::new();
    for evidence in [
        Evidence::MissingOverwrites,
        Evidence::Public,
        Evidence::Duplicate,
        Evidence::Contradictory,
        Evidence::ExtraViewer,
    ] {
        failures.extend(invalid_flow(evidence).await);
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[tokio::test]
async fn malformed_2xx_keeps_uncertainty_without_retry_or_destructive_cleanup() {
    let failures = invalid_flow(Evidence::MalformedJson).await;
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[tokio::test]
async fn valid_private_receipt_activates_and_posts_controls_exactly_once() {
    let Some(db) = fixture().await else {
        return;
    };
    let (mock, topic) = mock(Evidence::Valid).await;
    let tickets = runtime(db.pool().clone(), &mock);
    let result = tickets.execute(&open_button(), TicketAction::Open).await;
    let successful = result.is_ok();
    let before = rows(db.pool()).await;
    let _ = tickets
        .execute(&another_open_button(), TicketAction::Open)
        .await;
    let after = rows(db.pool()).await;
    let requests = mock.requests();
    let controls: Vec<_> = requests
        .iter()
        .filter(|r| r.method == "POST" && r.path.ends_with("/channels/600/messages"))
        .collect();
    let correct_controls = controls.len() == 1 && {
        let body: Value = serde_json::from_slice(&controls[0].body).unwrap();
        body["allowed_mentions"]["users"] == json!(["500"])
            && body["components"][0]["components"][0]["custom_id"] == "two:tickets:claim"
            && body["components"][0]["components"][1]["custom_id"] == "two:tickets:close"
    };
    let reserved_topic = topic.lock().unwrap().clone();
    mock.shutdown().await;
    drop(tickets);
    db.close().await.expect("teardown owned test database");
    assert!(successful, "valid receipt must preserve success");
    assert_eq!(before.len(), 1);
    assert_eq!(before[0].1, "open");
    assert_eq!(before[0].2.as_deref(), Some("600"));
    assert_eq!(reserved_topic, format!("two-ticket:{}", before[0].0));
    assert_eq!(before, after, "repeat open must preserve ownership");
    assert!(
        correct_controls,
        "controls once, with opener-only allowed mention"
    );
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.method == "POST" && r.path.ends_with("/guilds/100/channels"))
            .count(),
        1
    );
    assert!(requests
        .iter()
        .all(|r| r.method != "DELETE" && r.method != "PUT"));
}

async fn recover(evidence: Evidence, recorded: bool) -> Vec<String> {
    let Some(db) = fixture().await else {
        return Vec::new();
    };
    let mock = MockRest::with_responder(move |request| {
        match (request.method.as_str(), request.path.as_str()) {
            // Listing identifies a candidate, not authority to delete it. Fetch evidence
            // must still match the reservation and expected private-channel contract.
            ("GET", "/api/v10/guilds/100/channels") => ScriptedResponse::json(
                200,
                json!([receipt("two-ticket:recovery-reservation", Evidence::Valid)]),
            ),
            ("GET", "/api/v10/channels/600") => {
                response("two-ticket:recovery-reservation", evidence, 200)
            }
            ("DELETE", "/api/v10/channels/600") => ScriptedResponse::status(204),
            _ => ScriptedResponse::status(500),
        }
    })
    .await;
    let tickets = runtime(db.pool().clone(), &mock);
    let old = now_millis_for_test() - INTERRUPTED_AFTER_MS - 1;
    assert!(matches!(
        tickets
            .store
            .reserve("recovery-reservation", "500", old, 0)
            .await
            .unwrap(),
        OpenResult::Created(_)
    ));
    if recorded {
        tickets
            .store
            .record_channel("recovery-reservation", "600")
            .await
            .unwrap();
    }
    let stale = tickets
        .store
        .get("recovery-reservation")
        .await
        .unwrap()
        .unwrap();
    let _ = tickets.recover_ticket(&stale).await;
    let after = tickets.store.get("recovery-reservation").await.unwrap();
    let requests = mock.requests();
    let mut failures = Vec::new();
    if matches!(evidence, Evidence::Valid) {
        if after
            .as_ref()
            .is_none_or(|row| row.status != TicketStatus::Closed)
            || requests.iter().filter(|r| r.method == "DELETE").count() != 1
        {
            failures.push("proven private recovery no longer closes safely".into());
        }
    } else {
        if after
            .as_ref()
            .is_none_or(|row| row.status != TicketStatus::Creating)
        {
            failures.push(format!(
                "{evidence:?}/{recorded}: uncertain recovery abandoned or activated"
            ));
        }
        if requests.iter().any(|r| r.method != "GET") {
            failures.push(format!(
                "{evidence:?}/{recorded}: recovery deleted or mutated unproven channel"
            ));
        }
    }
    if requests.iter().any(|r| r.method == "POST") || requests.len() > 3 {
        failures.push(format!(
            "{evidence:?}/{recorded}: recovery recreated or exceeded read budget"
        ));
    }
    mock.shutdown().await;
    drop(tickets);
    db.close().await.expect("teardown owned test database");
    failures
}

#[tokio::test]
async fn recovery_never_deletes_an_unproven_candidate_even_if_its_id_was_recorded() {
    let mut failures = Vec::new();
    for recorded in [false, true] {
        for evidence in [
            Evidence::WrongGuild,
            Evidence::WrongType,
            Evidence::WrongParent,
            Evidence::WrongTopic,
            Evidence::MissingTopic,
            Evidence::Public,
            Evidence::MalformedJson,
        ] {
            failures.extend(recover(evidence, recorded).await);
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[tokio::test]
async fn proven_private_recovery_keeps_existing_close_without_duplicate_create() {
    let failures = recover(Evidence::Valid, false).await;
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

type DiagnosticFields = std::collections::BTreeMap<String, String>;

#[derive(Clone, Default)]
struct Diagnostics(Arc<TaskMutex<Vec<DiagnosticFields>>>);

#[derive(Default)]
struct Fields(DiagnosticFields);

impl tracing::field::Visit for Fields {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.insert(field.name().into(), format!("{value:?}"));
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Diagnostics {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        let mut fields = Fields::default();
        event.record(&mut fields);
        self.0.lock().unwrap().push(fields.0);
    }
}

#[tokio::test]
async fn shared_handler_reports_uncertainty_with_bounded_redacted_structured_next_action() {
    use tracing::instrument::WithSubscriber;
    use tracing_subscriber::prelude::*;

    let Some(db) = fixture().await else {
        return;
    };
    let (mock, _) = mock(Evidence::WrongTopic).await;
    let tickets = runtime(db.pool().clone(), &mock);
    let commands = crate::command_runtime::CommandRuntime::with_tickets(
        db.pool().clone(),
        tickets.executor.clone(),
        Arc::clone(&tickets),
    );
    let diagnostics = Diagnostics::default();
    let subscriber = tracing_subscriber::registry().with(diagnostics.clone());
    commands
        .on_interaction(&open_button())
        .with_subscriber(subscriber)
        .await;
    let requests = mock.requests();
    let replies: Vec<Value> = requests
        .iter()
        .filter(|r| r.method == "PATCH" && r.path.ends_with("/messages/@original"))
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    let events = diagnostics.0.lock().unwrap().clone();
    let rendered = format!("{events:?}");
    let callback: Vec<Value> = requests
        .iter()
        .filter(|r| r.path.ends_with("/callback"))
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    mock.shutdown().await;
    drop(commands);
    drop(tickets);
    db.close().await.expect("teardown owned test database");

    assert_eq!(callback.len(), 1);
    assert_eq!(callback[0]["type"], 5);
    assert_eq!(
        callback[0]["data"]["flags"], 64,
        "ephemeral acknowledgement"
    );
    assert_eq!(replies.len(), 1);
    assert!(
        safe_uncertain_reply(replies[0]["content"].as_str().unwrap()),
        "member must not receive ready/links/private payloads on invalid evidence"
    );
    for sentinel in [
        "receipt-private-channel-sentinel",
        "another-reservation",
        "receipt-interaction-token-sentinel",
        "receipt-member-sentinel",
        "receipt-fixture-token",
    ] {
        assert!(
            !rendered.contains(sentinel),
            "diagnostic echoed sensitive fixture data"
        );
    }
    // Do not dictate field names or exact copy. Require a ticket diagnostic with
    // machine-readable context and a bounded recovery/inspection next action.
    assert!(
        events.iter().any(|fields| {
            let text = format!("{fields:?}").to_lowercase();
            let structured = fields
                .keys()
                .filter(|key| key.as_str() != "message")
                .count()
                >= 2;
            structured
                && text.contains("ticket")
                && text.len() <= 1024
                && ["recover", "inspect", "staff", "reconcile"]
                    .iter()
                    .any(|word| text.contains(word))
        }),
        "missing bounded structured diagnostic with next action: {rendered}"
    );
}
