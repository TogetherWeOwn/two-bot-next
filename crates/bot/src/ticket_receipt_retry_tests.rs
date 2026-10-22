//! Retry/reattachment proof checks, separate from the immutable creation contract.
#![cfg(test)]

use super::*;

#[derive(Clone, Copy, Debug)]
enum Evidence {
    Valid,
    WrongParent,
    WrongTopic,
    MissingTopic,
    Public,
    MissingEveryone,
    MissingStaff,
    WrongGuild,
    WrongType,
}

fn receipt(topic: &str, evidence: Evidence) -> Value {
    let mut body = private_channel(topic);
    match evidence {
        Evidence::Valid => {}
        Evidence::WrongParent => body["parent_id"] = json!("201"),
        Evidence::WrongTopic => body["topic"] = json!("two-ticket:another-reservation"),
        Evidence::MissingTopic => {
            body.as_object_mut().unwrap().remove("topic");
        }
        Evidence::Public => {
            body["permission_overwrites"][0]["deny"] = json!("0");
            body["permission_overwrites"][0]["allow"] =
                json!(Permissions::VIEW_CHANNEL.bits().to_string());
        }
        Evidence::MissingEveryone => {
            body["permission_overwrites"]
                .as_array_mut()
                .unwrap()
                .remove(0);
        }
        Evidence::MissingStaff => {
            body["permission_overwrites"].as_array_mut().unwrap().pop();
        }
        Evidence::WrongGuild => body["guild_id"] = json!("101"),
        Evidence::WrongType => body["type"] = json!(2),
    }
    body
}

const UNPROVEN: [Evidence; 8] = [
    Evidence::WrongParent,
    Evidence::WrongTopic,
    Evidence::MissingTopic,
    Evidence::Public,
    Evidence::MissingEveryone,
    Evidence::MissingStaff,
    Evidence::WrongGuild,
    Evidence::WrongType,
];

// A cached channel id must not authorize a later DELETE after the channel's
// ownership/privacy evidence changes. The same pending row can close once the
// full proof is restored, without transcript capture or duplicate creation.
#[tokio::test]
#[ignore = "requires agent-testdb or CI service container"]
async fn cleanup_pending_open_rollback_revalidates_proof_on_each_retry() {
    let mut failures = Vec::new();
    for evidence in UNPROVEN {
        let db = TestDb::new().await;
        let current = Arc::new(std::sync::Mutex::new(receipt(
            "two-ticket:rollback",
            evidence,
        )));
        let served = Arc::clone(&current);
        let mock = MockRest::with_responder(move |request| {
            match (request.method.as_str(), request.path.as_str()) {
                ("GET", "/api/v10/channels/600") => {
                    ScriptedResponse::json(200, served.lock().unwrap().clone())
                }
                ("DELETE", "/api/v10/channels/600") => ScriptedResponse::status(204),
                _ => ScriptedResponse::status(500),
            }
        })
        .await;
        let tickets = runtime(db.pool.clone(), &mock);
        tickets
            .store
            .reserve("rollback", "500", 0, 0)
            .await
            .unwrap();
        tickets
            .store
            .record_channel("rollback", "600")
            .await
            .unwrap();
        // Crash after controls failed and cleanup was durably scheduled. No
        // saved close/transcript exists: this is specifically open rollback.
        sqlx::query("UPDATE tickets SET status='cleanup_pending' WHERE id='rollback'")
            .execute(&db.pool)
            .await
            .unwrap();
        let pending = tickets.store.get("rollback").await.unwrap().unwrap();
        let result = tickets.recover_ticket(&pending).await;
        let after = tickets.store.get("rollback").await.unwrap().unwrap();
        let requests = mock.requests();
        if result.is_ok() || after.status != TicketStatus::CleanupPending {
            failures.push(format!(
                "{evidence:?}: unproven rollback did not stay pending"
            ));
        }
        if requests.len() != 1 || requests.iter().any(|r| r.method != "GET") {
            failures.push(format!("{evidence:?}: unproven rollback mutated Discord"));
        }
        *current.lock().unwrap() = receipt("two-ticket:rollback", Evidence::Valid);
        let retry = tickets.recover_ticket(&pending).await;
        let after = tickets.store.get("rollback").await.unwrap().unwrap();
        let requests = mock.requests();
        if retry.is_err() || after.status != TicketStatus::Closed {
            failures.push(format!(
                "{evidence:?}: restored proof did not close rollback"
            ));
        }
        if requests.iter().filter(|r| r.method == "DELETE").count() != 1
            || requests
                .iter()
                .any(|r| r.method == "POST" || r.method == "PUT")
        {
            failures.push(format!("{evidence:?}: rollback cleanup was not one DELETE"));
        }
        if tickets.store.transcript_exists("rollback").await.unwrap() {
            failures.push(format!("{evidence:?}: rollback invented a transcript"));
        }
        mock.shutdown().await;
        db.close().await;
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

fn own_controls() -> Value {
    json!({"id":"900", "author":{"id":"400"}, "components":[
        {"type":1, "components":[
            {"type":2,"custom_id":TICKET_CLAIM_ID},
            {"type":2,"custom_id":TICKET_CLOSE_ID}
        ]}
    ]})
}

async fn recover_open(evidence: Evidence) -> Vec<String> {
    let db = TestDb::new().await;
    let posted = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let controls = Arc::clone(&posted);
    let mock = MockRest::with_responder(move |request| {
        match (request.method.as_str(), request.path.as_str()) {
            ("GET", "/api/v10/guilds/100/channels/200") | ("GET", "/api/v10/channels/200") => {
                ScriptedResponse::json(200, json!({"id":"200","guild_id":"100","type":4}))
            }
            ("GET", "/api/v10/channels/600") => {
                ScriptedResponse::json(200, receipt("two-ticket:reattach", evidence))
            }
            ("GET", "/api/v10/channels/600/messages?limit=50") => {
                let messages = if controls.load(Ordering::SeqCst) {
                    json!([own_controls()])
                } else {
                    json!([])
                };
                ScriptedResponse::json(200, messages)
            }
            ("POST", "/api/v10/channels/600/messages") => {
                controls.store(true, Ordering::SeqCst);
                ScriptedResponse::json(200, json!({"id":"900"}))
            }
            _ => ScriptedResponse::status(500),
        }
    })
    .await;
    let tickets = runtime(db.pool.clone(), &mock);
    let ticket = active(&tickets, "reattach").await;
    let result = tickets.recover_ticket(&ticket).await;
    let mut failures = Vec::new();
    if matches!(evidence, Evidence::Valid) {
        if result.is_err() || tickets.recover_ticket(&ticket).await.is_err() {
            failures.push("full proof failed to restore/idempotently retain controls".into());
        }
        if mock
            .requests()
            .iter()
            .filter(|r| r.method == "POST")
            .count()
            != 1
        {
            failures.push("full proof must post missing controls exactly once".into());
        }
    } else {
        if result.is_ok() {
            failures.push(format!(
                "{evidence:?}: unproven Open recovery reported success"
            ));
        }
        if mock.requests().iter().any(|r| r.method != "GET") {
            failures.push(format!(
                "{evidence:?}: unproven Open recovery posted or mutated"
            ));
        }
    }
    let after = tickets.store.get("reattach").await.unwrap().unwrap();
    if after.status != TicketStatus::Open || after.channel_id.as_deref() != Some("600") {
        failures.push(format!(
            "{evidence:?}: control recovery changed the Open row"
        ));
    }
    if tickets.store.transcript_exists("reattach").await.unwrap() {
        failures.push(format!(
            "{evidence:?}: control recovery invented a transcript"
        ));
    }
    if mock
        .requests()
        .iter()
        .any(|r| r.method == "DELETE" || r.method == "PUT")
    {
        failures.push(format!(
            "{evidence:?}: control recovery performed unrelated mutation"
        ));
    }
    mock.shutdown().await;
    db.close().await;
    failures
}

// Moving proof validation after ensure_controls would expose controls in a
// foreign/reparented/public channel even though the persisted row is Open.
#[tokio::test]
#[ignore = "requires agent-testdb or CI service container"]
async fn open_missing_controls_refuses_unproven_receipts_before_posting() {
    let mut failures = Vec::new();
    for evidence in UNPROVEN {
        failures.extend(recover_open(evidence).await);
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI service container"]
async fn open_missing_controls_with_full_proof_posts_once() {
    let failures = recover_open(Evidence::Valid).await;
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
