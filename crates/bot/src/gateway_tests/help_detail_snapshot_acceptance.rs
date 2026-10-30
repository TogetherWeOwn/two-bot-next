//! Additional detail checks run by the existing confirmed-publication journey.
//! Capture every stage before asserting new UX so expected RED still tears down
//! the isolated schema and loopback services.
#![cfg(test)]

use super::*;

async fn put(db: &TestDb, name: &str, description: &str, audit: &str) {
    two_bot_core::custom_command_service::put(
        &db.pool,
        true,
        GUILD,
        "3333",
        &two_bot_core::custom_commands::PutCommandInput {
            name: name.into(),
            description: description.into(),
            template: "DO NOT EXECUTE THIS TEMPLATE".into(),
            text_trigger: None,
        },
        audit,
        &two_bot_core::now_iso(),
    )
    .await
    .expect("fixture custom revision");
}

async fn rows(db: &TestDb) -> (Vec<(String, String)>, Vec<(String, String)>) {
    let commands = sqlx::query_as(
        "SELECT to_jsonb(c)::text, c.xmin::text FROM automation_commands c ORDER BY name",
    )
    .fetch_all(&db.pool)
    .await
    .expect("command rows and MVCC versions");
    let audit = sqlx::query_as(
        "SELECT to_jsonb(a)::text, a.xmin::text FROM automation_audit_log a ORDER BY id",
    )
    .fetch_all(&db.pool)
    .await
    .expect("audit rows and MVCC versions");
    (commands, audit)
}

async fn detail(
    db: &TestDb,
    runtime: &Arc<crate::command_runtime::CommandRuntime>,
    rest: &MockRest,
    id: u64,
    selector: &str,
) -> String {
    let before = rows(db).await;
    // All connections are unavailable to help. A new read, audit or execution
    // prerequisite must not postpone even the immediate callback.
    let mut held = vec![];
    for _ in 0..3 {
        held.push(db.pool.acquire().await.expect("hold fixture pool"));
    }
    let mut packet = slash(0, id);
    packet["d"]["data"]["name"] = json!("help");
    packet["d"]["data"]["options"] = json!([{"name":"command", "type":3, "value":selector}]);
    let input = serde_json::from_value(packet["d"].take()).expect("help selector interaction");
    let count = rest.requests().len();
    tokio::time::timeout(Duration::from_millis(250), runtime.on_interaction(&input))
        .await
        .expect("help must not wait for pool or in-flight registry lock");
    drop(held);
    assert_eq!(
        before,
        rows(db).await,
        "help changes no command/audit row or MVCC version"
    );
    let requests = rest.requests();
    assert_eq!(
        requests.len(),
        count + 1,
        "help adds only its callback, never GET/PUT/execution"
    );
    let request = &requests[count];
    assert_eq!(request.method, "POST");
    assert!(request.path.ends_with("/callback"));
    let reply: Value = serde_json::from_slice(&request.body).expect("help callback");
    assert_eq!(reply["type"], 4);
    assert_eq!(reply["data"]["flags"], 64);
    assert_eq!(reply["data"]["allowed_mentions"]["parse"], json!([]));
    let mut text = reply["data"]["content"].as_str().unwrap_or("").to_owned();
    if let Some(embeds) = reply["data"]["embeds"].as_array() {
        for embed in embeds {
            if let Some(desc) = embed["description"].as_str() {
                text.push_str(desc);
            }
        }
    }
    assert!(text.encode_utf16().count() <= 6000);
    text
}

pub(super) struct Observed {
    old: String,
    in_flight: String,
    confirmed: String,
    failed: String,
    hidden: String,
    removed: String,
}

impl Observed {
    pub(super) fn assert_expected(self) {
        for text in [&self.old, &self.in_flight] {
            assert!(
                text.contains("Confirmed old help revision"),
                "explain old confirmed description: {text}"
            );
            assert!(!text.contains("Confirmed new help revision"));
        }
        for text in [&self.confirmed, &self.failed] {
            assert!(
                text.contains("Confirmed new help revision"),
                "confirmed revision must be readable: {text}"
            );
            assert!(!text.contains("UNCONFIRMED failed revision"));
        }
        assert!(!self.hidden.contains("Unpublished ghost description"));
        assert!(!self.hidden.contains("/ghostfixture"));
        assert!(self.hidden.to_lowercase().contains("confirmed") && self.hidden.contains("/help"));
        assert!(!self.removed.contains("Confirmed new help revision"));
        assert!(!self.removed.contains("/faq"));
        assert!(self.removed.contains("/help"));
    }
}

pub(super) async fn capture(db: &TestDb) -> Observed {
    put(db, "faq", "Confirmed old help revision", "help-old").await;
    let mut script = bootstrap_responses();
    script.extend([
        ScriptedResponse::status(200), // initial confirmed PUT
        ScriptedResponse::status(204), // old help
        ScriptedResponse::status(200).delayed(Duration::from_millis(900)), // next PUT held in flight
        ScriptedResponse::status(204),                                     // help while PUT is held
        ScriptedResponse::status(204),                                     // new confirmed help
        ScriptedResponse::status(403), // failed publication, no retry
        ScriptedResponse::status(204), // help keeps confirmed definition
        ScriptedResponse::status(204), // unpublished ghost correction
        ScriptedResponse::status(200), // confirmed removal
        ScriptedResponse::status(204), // removed command correction
    ]);
    let rest = MockRest::start(script, ScriptedResponse::status(500)).await;
    let runtime = bootstrap(db, &rest, &vars(Some("0"))).await;
    runtime
        .publish_registry_checked(Some(1111))
        .await
        .expect("initial publication");
    let first = runtime.published_commands().expect("old snapshot");
    let old = detail(db, &runtime, &rest, 91, "faq").await;
    put(db, "faq", "Confirmed new help revision", "help-new").await;
    let publishing = {
        let runtime = Arc::clone(&runtime);
        tokio::spawn(async move { runtime.publish_registry_checked(Some(1111)).await })
    };
    wait_requests(&rest, 5).await;
    assert!(Arc::ptr_eq(
        &first,
        &runtime
            .published_commands()
            .expect("in-flight old snapshot")
    ));
    let in_flight = detail(db, &runtime, &rest, 92, "faq").await;
    publishing
        .await
        .expect("publisher join")
        .expect("new receipt");
    let second = runtime.published_commands().expect("new snapshot");
    assert!(!Arc::ptr_eq(&first, &second));
    assert_eq!(
        first
            .iter()
            .find(|def| def.name == "faq")
            .expect("old faq")
            .description,
        "Confirmed old help revision",
        "old Arc is immutable"
    );
    let confirmed = detail(db, &runtime, &rest, 93, "faq").await;
    put(db, "faq", "UNCONFIRMED failed revision", "help-failed").await;
    put(
        db,
        "ghostfixture",
        "Unpublished ghost description",
        "help-ghost",
    )
    .await;
    assert!(runtime.publish_registry_checked(Some(1111)).await.is_err());
    assert!(Arc::ptr_eq(
        &second,
        &runtime
            .published_commands()
            .expect("failed retains confirmed")
    ));
    let failed = detail(db, &runtime, &rest, 94, "faq").await;
    let hidden = detail(db, &runtime, &rest, 95, "ghostfixture").await;
    two_bot_core::custom_command_service::delete(
        &db.pool,
        true,
        GUILD,
        "3333",
        "faq",
        "help-remove",
        &two_bot_core::now_iso(),
    )
    .await
    .expect("fixture removal");
    runtime
        .publish_registry_checked(Some(1111))
        .await
        .expect("confirmed removal");
    let removed = detail(db, &runtime, &rest, 96, "faq").await;
    assert_eq!(
        rest.requests().len(),
        12,
        "bootstrap, publications and six callbacks only"
    );
    rest.shutdown().await;
    drop(runtime);
    Observed {
        old,
        in_flight,
        confirmed,
        failed,
        hidden,
        removed,
    }
}
