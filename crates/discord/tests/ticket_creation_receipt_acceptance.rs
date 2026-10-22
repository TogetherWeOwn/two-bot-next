//! Protected acceptance: a successful create status is not private-channel proof.
//! Fixtures are independent of the request builder; only loopback HTTP is used.
#[allow(dead_code)]
mod common;

use common::{MockRest, ScriptedResponse};
use serde_json::{json, Value};
use two_bot_discord::{executor::TicketChannelRequest, ActionExecutor};

fn request() -> TicketChannelRequest<'static> {
    TicketChannelRequest {
        guild_id: "100",
        category_id: "200",
        staff_role_id: "300",
        bot_id: "400",
        opener_id: "500",
        username: "Member",
        reservation_id: "receipt-reservation",
    }
}

fn receipt() -> Value {
    // VIEW=1024, SEND=2048, HISTORY=65536, MANAGE_CHANNELS=16.
    json!({
        "id":"600", "guild_id":"100", "type":0, "parent_id":"200",
        "topic":"two-ticket:receipt-reservation",
        "permission_overwrites":[
            {"id":"100","type":0,"allow":"0","deny":"1024"},
            {"id":"400","type":1,"allow":"68624","deny":"0"},
            {"id":"500","type":1,"allow":"68608","deny":"0"},
            {"id":"300","type":0,"allow":"68608","deny":"0"}
        ]
    })
}

async fn refused(cases: Vec<(&str, ScriptedResponse)>) {
    let mut violations = Vec::new();
    for (name, response) in cases {
        // A fetch is permitted, but cannot turn repeated unproven evidence into success.
        let mock = MockRest::start(vec![response.clone()], response).await;
        let executor =
            ActionExecutor::with_proxy("receipt-fixture-token".into(), Some(mock.origin()))
                .expect("loopback executor");
        match executor.create_ticket_channel(&request()).await {
            Ok(_) => violations.push(format!("{name}: unproven receipt accepted")),
            Err(error) => {
                if error.is_safe_pre_mutation() {
                    violations.push(format!("{name}: 2xx mislabeled proven unsent"));
                }
                let diagnostic = error.to_string();
                if diagnostic.len() > 512 || diagnostic.contains("receipt-private-sentinel") {
                    violations.push(format!("{name}: unbounded or leaking diagnostic"));
                }
            }
        }
        let requests = mock.requests();
        if requests.iter().filter(|r| r.method == "POST").count() != 1
            || requests.iter().filter(|r| r.method == "GET").count() > 1
            || requests.iter().any(|r| {
                !(r.method == "POST" && r.path == "/api/v10/guilds/100/channels"
                    || r.method == "GET" && r.path == "/api/v10/channels/600")
            })
        {
            violations.push(format!("{name}: unexpected mutation or unbounded fetch"));
        }
        mock.shutdown().await;
    }
    assert!(violations.is_empty(), "{}", violations.join("\n"));
}

#[tokio::test]
async fn full_private_receipt_keeps_one_create_and_the_requested_permission_contract() {
    let mock = MockRest::start(vec![], ScriptedResponse::json(201, receipt())).await;
    let executor = ActionExecutor::with_proxy("receipt-fixture-token".into(), Some(mock.origin()))
        .expect("loopback executor");
    assert_eq!(
        executor.create_ticket_channel(&request()).await.unwrap(),
        "600"
    );
    let requests = mock.requests();
    let posts: Vec<_> = requests.iter().filter(|r| r.method == "POST").collect();
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0].path, "/api/v10/guilds/100/channels");
    let body: Value = serde_json::from_slice(&posts[0].body).unwrap();
    for field in ["type", "parent_id", "topic"] {
        assert_eq!(body[field], receipt()[field], "request {field}");
    }
    let sorted = |value: &Value| {
        let mut rows = value["permission_overwrites"].as_array().unwrap().clone();
        rows.sort_by_key(|row| {
            (
                row["id"].as_str().unwrap().to_owned(),
                row["type"].as_u64().unwrap(),
            )
        });
        rows
    };
    assert_eq!(
        sorted(&body),
        sorted(&receipt()),
        "permission contract, not wire order"
    );
    assert!(requests.len() <= 2, "at most one corroborating fetch");
    assert!(requests
        .iter()
        .all(|r| r.method != "PUT" && r.method != "DELETE"));
    mock.shutdown().await;
}

#[tokio::test]
async fn missing_or_wrong_identity_metadata_never_proves_creation() {
    let mut cases = Vec::new();
    for (field, wrong) in [
        ("guild_id", json!("101")),
        ("type", json!(4)),
        ("parent_id", json!("201")),
        ("topic", json!("two-ticket:other-reservation")),
    ] {
        let mut missing = receipt();
        missing.as_object_mut().unwrap().remove(field);
        cases.push((field, ScriptedResponse::json(201, missing)));
        let mut mismatch = receipt();
        mismatch[field] = wrong;
        cases.push((field, ScriptedResponse::json(201, mismatch)));
    }
    cases.push(("id only", ScriptedResponse::json(201, json!({"id":"600"}))));
    refused(cases).await;
}

#[tokio::test]
async fn missing_public_duplicate_or_contradictory_overwrites_are_uncertain() {
    let mut cases = Vec::new();
    for index in 0..4 {
        let mut missing = receipt();
        missing["permission_overwrites"]
            .as_array_mut()
            .unwrap()
            .remove(index);
        cases.push((
            "missing required principal",
            ScriptedResponse::json(201, missing),
        ));
        let mut duplicate = receipt();
        let row = duplicate["permission_overwrites"][index].clone();
        duplicate["permission_overwrites"]
            .as_array_mut()
            .unwrap()
            .push(row);
        cases.push((
            "duplicate principal",
            ScriptedResponse::json(201, duplicate),
        ));
        let mut contradictory = receipt();
        contradictory["permission_overwrites"][index]["deny"] = json!("1024");
        contradictory["permission_overwrites"][index]["allow"] = json!("68608");
        cases.push((
            "allow and deny view",
            ScriptedResponse::json(201, contradictory),
        ));
    }
    for (name, overwrites) in [
        ("missing array", Value::Null),
        ("empty array", json!([])),
        ("object not array", json!({})),
    ] {
        let mut body = receipt();
        body["permission_overwrites"] = overwrites;
        cases.push((name, ScriptedResponse::json(201, body)));
    }
    let mut public = receipt();
    public["permission_overwrites"][0]["deny"] = json!("0");
    cases.push((
        "everyone view not denied",
        ScriptedResponse::json(201, public),
    ));
    for (index, allow) in [(1, "68608"), (2, "66560"), (3, "3072")] {
        let mut body = receipt();
        body["permission_overwrites"][index]["allow"] = json!(allow);
        cases.push(("required access lost", ScriptedResponse::json(201, body)));
    }
    for (name, row) in [
        (
            "extra role visibility",
            json!({"id":"301","type":0,"allow":"1024","deny":"0"}),
        ),
        (
            "extra member visibility",
            json!({"id":"501","type":1,"allow":"1024","deny":"0"}),
        ),
        (
            "wrong principal kind",
            json!({"id":"500","type":0,"allow":"68608","deny":"0"}),
        ),
        (
            "malformed bits",
            json!({"id":"301","type":0,"allow":"receipt-private-sentinel","deny":"0"}),
        ),
        (
            "numeric bits",
            json!({"id":"301","type":0,"allow":1024,"deny":"0"}),
        ),
        (
            "overflow bits",
            json!({"id":"301","type":0,"allow":"18446744073709551616","deny":"0"}),
        ),
    ] {
        let mut body = receipt();
        body["permission_overwrites"]
            .as_array_mut()
            .unwrap()
            .push(row);
        cases.push((name, ScriptedResponse::json(201, body)));
    }
    refused(cases).await;
}

#[tokio::test]
async fn malformed_success_bodies_never_become_safe_pre_mutation_errors() {
    let mut malformed = ScriptedResponse::status(201);
    malformed.body = b"{\"id\":\"600\",receipt-private-sentinel".to_vec();
    refused(vec![
        ("invalid json", malformed),
        ("empty 204", ScriptedResponse::status(204)),
        ("null", ScriptedResponse::json(200, Value::Null)),
        ("array", ScriptedResponse::json(201, json!([receipt()]))),
        ("zero id", ScriptedResponse::json(201, json!({"id":"0"}))),
    ])
    .await;
}

#[tokio::test]
async fn incomplete_receipt_can_succeed_only_after_one_matching_private_fetch() {
    let mock = MockRest::start(
        vec![ScriptedResponse::json(201, json!({"id":"600"}))],
        ScriptedResponse::json(200, receipt()),
    )
    .await;
    let executor = ActionExecutor::with_proxy("receipt-fixture-token".into(), Some(mock.origin()))
        .expect("loopback executor");
    let result = executor.create_ticket_channel(&request()).await;
    let requests = mock.requests();
    let corroborated = requests.len() == 2
        && requests[1].method == "GET"
        && requests[1].path == "/api/v10/channels/600";
    let bounded_trace = !requests.is_empty()
        && requests.len() <= 2
        && requests[0].method == "POST"
        && requests[0].path == "/api/v10/guilds/100/channels"
        && requests
            .iter()
            .skip(1)
            .all(|r| r.method == "GET" && r.path == "/api/v10/channels/600");
    let safe_result = match result {
        Ok(id) => id == "600" && corroborated,
        Err(error) => !error.is_safe_pre_mutation(),
    };
    mock.shutdown().await;
    assert!(
        bounded_trace,
        "refusal and corroboration both forbid retry/delete/unbounded fetch"
    );
    assert!(
        safe_result,
        "id-only success needs fetched identity and privacy evidence"
    );
}

#[tokio::test]
async fn a_matching_fetch_cannot_launder_a_contradictory_create_receipt() {
    let mut wrong = receipt();
    wrong["guild_id"] = json!("101");
    let mock = MockRest::start(
        vec![ScriptedResponse::json(201, wrong)],
        ScriptedResponse::json(200, receipt()),
    )
    .await;
    let executor = ActionExecutor::with_proxy("receipt-fixture-token".into(), Some(mock.origin()))
        .expect("loopback executor");
    let result = executor.create_ticket_channel(&request()).await;
    let refused_as_uncertain = result.is_err_and(|error| !error.is_safe_pre_mutation());
    assert_eq!(
        mock.requests()
            .iter()
            .filter(|r| r.method == "POST")
            .count(),
        1
    );
    assert!(mock
        .requests()
        .iter()
        .all(|r| r.method == "POST" || r.method == "GET"));
    mock.shutdown().await;
    assert!(
        refused_as_uncertain,
        "contradictory evidence must fail closed"
    );
}
