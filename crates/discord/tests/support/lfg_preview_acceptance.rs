//! Protected preview contract, exercised through the production slash dispatcher.
//! A preview accidentally routed as Create must fail these assertions, even if
//! store errors are swallowed: the SQL tripwire and REST transcript observe it.

use super::*;
use std::time::Duration;

const START: &str = "2099-09-11T20:00:00Z";

fn command(id: u64, title: &str, start: &str, roles: &str, preview: &[Value]) -> Interaction {
    let mut options = vec![
        json!({"type": 3, "name": "title", "value": title}),
        json!({"type": 3, "name": "starts-at", "value": start}),
        json!({"type": 3, "name": "roles", "value": roles}),
    ];
    options.extend_from_slice(preview);
    invocation(
        id,
        3333,
        PERM_MANAGE_EVENTS,
        json!({"id": "1", "name": "lfg", "type": 1, "options": options}),
        2,
    )
}

fn flag(value: bool) -> Value {
    json!({"type": 5, "name": "preview", "value": value})
}

async fn rest() -> MockRest {
    MockRest::with_responder(|request| match (request.method.as_str(), request.path.as_str()) {
        ("POST", path) if path.ends_with("/callback") => ScriptedResponse::status(204),
        ("PATCH", "/api/v10/webhooks/1111/lfg-mock-token/messages/@original") => {
            ScriptedResponse::json(200, json!({"id": "5900"}))
        }
        ("POST", "/api/v10/channels/4444/messages") => {
            ScriptedResponse::json(200, json!({"id": "5000"}))
        }
        ("GET", "/api/v10/users/@me") => ScriptedResponse::json(
            200,
            json!({"id": "1111", "username": "fixture", "discriminator": "0", "avatar": null, "bot": true}),
        ),
        _ => ScriptedResponse::status(404),
    })
    .await
}

// The socket stays bound for the whole call. No released-port race and no
// credentials, real database, process environment, or external service involved.
// Any SQL access opens a connection and is caught even if the handler hides the
// error. A second fixture uses an already-closed pool to prove independence.
async fn tripwire_pool() -> (sqlx::PgPool, tokio::net::TcpListener) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind SQL tripwire");
    let options = sqlx::postgres::PgConnectOptions::new()
        .host("127.0.0.1")
        .port(listener.local_addr().unwrap().port())
        .username("preview_fixture")
        .database("preview_fixture")
        .ssl_mode(sqlx::postgres::PgSslMode::Disable);
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_millis(100))
        .connect_lazy_with(options);
    (pool, listener)
}

async fn closed_pool() -> sqlx::PgPool {
    let (pool, listener) = tripwire_pool().await;
    pool.close().await;
    drop(listener);
    pool
}

fn assert_no_sql(listener: tokio::net::TcpListener) {
    let listener = listener.into_std().expect("nonblocking SQL tripwire");
    assert!(
        matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
        "preview/refusal tried to connect to the database"
    );
}

fn private_result(requests: &[common::RestRequest]) -> Value {
    assert!(!requests.is_empty(), "a silent no-op is not a preview");
    assert!(
        requests.iter().all(|request| {
            (request.method == "POST" && request.path.ends_with("/callback"))
                || (request.method == "PATCH"
                    && request.path == "/api/v10/webhooks/1111/lfg-mock-token/messages/@original")
        }),
        "only interaction acknowledgement/completion is allowed: {:?}",
        requests
            .iter()
            .map(|r| (&r.method, &r.path))
            .collect::<Vec<_>>()
    );
    let callbacks: Vec<_> = requests
        .iter()
        .filter(|r| r.path.ends_with("/callback"))
        .collect();
    assert_eq!(callbacks.len(), 1, "exactly one private acknowledgement");
    let callback = body(callbacks[0]);
    assert_eq!(callback["data"]["flags"].as_u64().unwrap_or(0) & 64, 64);
    let response = match callback["type"].as_u64() {
        Some(4) => {
            assert_eq!(requests.len(), 1, "one immediate response, not a followup");
            callback["data"].clone()
        }
        Some(5) => {
            assert_eq!(
                requests.len(),
                2,
                "one deferred completion, not multiple replies"
            );
            body(&requests[1])
        }
        other => panic!("expected private message or defer, got {other:?}"),
    };
    assert_eq!(response["allowed_mentions"]["parse"], json!([]));
    for kind in ["users", "roles"] {
        assert!(
            response["allowed_mentions"][kind].is_null()
                || response["allowed_mentions"][kind] == json!([])
        );
    }
    assert_ne!(response["allowed_mentions"]["replied_user"], true);
    assert!(
        response["components"].is_null() || response["components"] == json!([]),
        "preview must not carry signup, publish, or draft components"
    );
    let content = response["content"].as_str().expect("visible content");
    assert!(!content.is_empty());
    assert!(
        content.encode_utf16().count() <= 2000,
        "Discord content limit"
    );
    response
}

fn assert_preview(response: &Value, id: u64) -> &str {
    let text = response["content"].as_str().unwrap();
    assert!(
        text.contains("Preview — nothing posted"),
        "missing preview disclosure: {text}"
    );
    assert!(!text.contains("LFG posted:"), "preview claims publication");
    assert!(
        !text.contains(&format!("lfg-{id}")),
        "synthetic persisted identity leaked"
    );
    assert!(
        !text.contains("LFG id:"),
        "preview must not carry the persisted-post identity footer"
    );
    assert!(!text.contains("custom_id") && !text.contains("__leave__"));
    text
}

async fn offline_reply(interaction: Interaction) -> Value {
    let (pool, listener) = tripwire_pool().await;
    let mock = rest().await;
    let rt = runtime(pool.clone(), &mock, true);
    rt.set_bot_user_id(0);
    rt.set_application_id(BOT);
    let handled = tokio::time::timeout(Duration::from_secs(5), rt.handle(&interaction)).await;
    let requests = mock.requests();
    mock.shutdown().await;
    drop(rt);
    pool.close().await;
    assert!(handled
        .expect("bounded local dispatch")
        .expect("dispatch result"));
    assert_no_sql(listener);
    private_result(&requests)
}

#[tokio::test]
async fn preview_valid_normalizes_roles_and_localizes_time_without_sql_or_identity() {
    let reply = offline_reply(command(
        9101,
        "  Friday raid  ",
        "2099-09-11T22:00:00+02:00",
        " TANK : Tank : 2 , healer : Healer : 1 ",
        &[flag(true)],
    ))
    .await;
    let text = assert_preview(&reply, 9101);
    assert!(
        text.contains(
            "**Friday raid** — Open\nStarts <t:4092840000:F>\n**Tank** 0/2\n**Healer** 0/1"
        ),
        "preview must reuse the canonical creation content: {text}"
    );
    assert!(
        text.contains("<t:4092840000:F>"),
        "Discord-localized UTC start: {text}"
    );
    assert!(!text.contains("2099-09-11T22:00:00+02:00"));
}

#[tokio::test]
async fn preview_works_with_an_already_closed_pool_and_unknown_bot_identity() {
    let pool = closed_pool().await;
    let mock = rest().await;
    let rt = runtime(pool, &mock, true);
    rt.set_bot_user_id(0);
    let handled = rt
        .handle(&command(9102, "Raid", START, "tank:Tank:2", &[flag(true)]))
        .await;
    let requests = mock.requests();
    mock.shutdown().await;
    assert!(handled.unwrap());
    assert_preview(&private_result(&requests), 9102);
}

#[tokio::test]
async fn preview_hostile_unicode_labels_are_safe_and_maximum_inputs_are_bounded() {
    let title = "😀".repeat(50); // 100 UTF-16 units: the accepted title boundary.
    let roles = (0..20)
        .map(|n| format!("{n:032}:{}:99", "😀".repeat(40)))
        .collect::<Vec<_>>()
        .join(",");
    assert_eq!(
        roles.encode_utf16().count(),
        2339,
        "exact role-spec input boundary"
    );
    let reply = offline_reply(command(9103, &title, START, &roles, &[flag(true)])).await;
    let text = assert_preview(&reply, 9103);
    assert!(text.contains(&title), "valid maximum title retained");
    let hostile = offline_reply(command(
        9104,
        "Raid @everyone <@3333>",
        START,
        "tank:坦克 @everyone:2,heal:治療 <@&5555>:1",
        &[flag(true)],
    ))
    .await;
    let text = assert_preview(&hostile, 9104);
    assert!(text.contains("坦克") && text.contains("治療"));
    assert!(
        !text.contains("@everyone") && !text.contains("<@3333>") && !text.contains("<@&5555>"),
        "raw mentions must not survive the shared renderer: {text}"
    );
}

#[tokio::test]
async fn preview_refuses_invalid_title_time_and_roles_without_store_dependency() {
    // Each invalid input has an otherwise-valid command. Removing any shared
    // validator must admit that case, rather than being masked by another error.
    let over_title = "😀".repeat(51);
    let over_label = "😀".repeat(41);
    let over_roles = (0..21)
        .map(|n| format!("r{n}:Role:1"))
        .collect::<Vec<_>>()
        .join(",");
    let cases = vec![
        (
            "empty title",
            "   ".to_owned(),
            START.to_owned(),
            "tank:Tank:1".to_owned(),
        ),
        (
            "overlong title",
            over_title,
            START.to_owned(),
            "tank:Tank:1".to_owned(),
        ),
        (
            "malformed time",
            "Raid".into(),
            "not-a-time @everyone".into(),
            "tank:Tank:1".into(),
        ),
        (
            "past time",
            "Raid".into(),
            "2000-01-01T00:00:00Z".into(),
            "tank:Tank:1".into(),
        ),
        (
            "duplicate role",
            "Raid".into(),
            START.into(),
            "TANK:Tank:1, tank :Other:2".into(),
        ),
        (
            "reserved role",
            "Raid".into(),
            START.into(),
            "__leave__:Leave:1".into(),
        ),
        (
            "overlong specification",
            "Raid".into(),
            START.into(),
            "x".repeat(2340),
        ),
        (
            "overlong key",
            "Raid".into(),
            START.into(),
            format!("{}:Tank:1", "x".repeat(33)),
        ),
        (
            "zero seats",
            "Raid".into(),
            START.into(),
            "tank:Tank:0".into(),
        ),
        (
            "too many seats",
            "Raid".into(),
            START.into(),
            "tank:Tank:100".into(),
        ),
        (
            "invalid key",
            "Raid".into(),
            START.into(),
            "@everyone:Tank:1".into(),
        ),
        ("empty label", "Raid".into(), START.into(), "tank::1".into()),
        (
            "overlong label",
            "Raid".into(),
            START.into(),
            format!("tank:{over_label}:1"),
        ),
        ("too many roles", "Raid".into(), START.into(), over_roles),
        (
            "missing role fields",
            "Raid".into(),
            START.into(),
            "tank".into(),
        ),
    ];
    for (n, (case, title, start, roles)) in cases.into_iter().enumerate() {
        let response = offline_reply(command(
            9200 + n as u64,
            &title,
            &start,
            &roles,
            &[flag(true)],
        ))
        .await;
        let text = response["content"].as_str().unwrap();
        assert!(
            !text.contains("Preview — nothing posted"),
            "{case} was accepted"
        );
        assert!(
            !text.contains("operation failed"),
            "{case} reached infrastructure instead of validation: {text}"
        );
        assert!(
            !text.contains("@everyone"),
            "hostile input echoed in refusal"
        );
        assert!(
            text.to_lowercase().contains(if case.contains("title") {
                "title"
            } else if case.contains("time") {
                "starts-at"
            } else {
                "role"
            }),
            "{case} must explain the invalid field: {text}"
        );
    }
}

#[tokio::test]
async fn preview_wrong_types_and_duplicate_options_fail_closed_instead_of_creating() {
    let cases = vec![
        vec![json!({"type":3,"name":"preview","value":"true"})],
        vec![json!({"type":4,"name":"preview","value":1})],
        vec![json!({"type":6,"name":"preview","value":"3333"})],
        vec![flag(true), flag(true)],
        vec![flag(false), flag(true)],
        vec![flag(true), flag(false)],
        vec![flag(false), flag(false)],
        vec![
            json!({"type":3,"name":"preview","value":"false"}),
            flag(true),
        ],
    ];
    for (n, options) in cases.into_iter().enumerate() {
        let response = offline_reply(command(
            9300 + n as u64,
            "Raid",
            START,
            "tank:Tank:1",
            &options,
        ))
        .await;
        let text = response["content"].as_str().unwrap();
        assert!(
            text.to_lowercase().contains("preview"),
            "name invalid option: {text}"
        );
        assert!(!text.contains("Preview — nothing posted") && !text.contains("LFG posted:"));
        assert!(
            !text.contains("operation failed"),
            "bad options must not become Create"
        );
    }
}

#[tokio::test]
async fn preview_preserves_guild_permission_feature_and_application_admission() {
    for case in [
        "permission",
        "missing permissions",
        "disabled",
        "foreign guild",
        "dm",
        "foreign application",
    ] {
        let (pool, listener) = tripwire_pool().await;
        let mock = rest().await;
        let rt = runtime(pool.clone(), &mock, case != "disabled");
        rt.set_bot_user_id(0);
        rt.set_application_id(BOT);
        let mut interaction = command(9401, "Raid", START, "tank:Tank:1", &[flag(true)]);
        if case == "permission" {
            interaction.member.as_mut().unwrap().permissions =
                Some(twilight_model::guild::Permissions::empty());
        }
        if case == "missing permissions" {
            interaction.member.as_mut().unwrap().permissions = None;
        }
        if case == "foreign guild" {
            interaction.guild_id = Some(twilight_model::id::Id::new(9999));
        }
        if case == "dm" {
            interaction.guild_id = None;
        }
        if case == "foreign application" {
            interaction.application_id = twilight_model::id::Id::new(BOT + 1);
        }
        let handled = rt.handle(&interaction).await.unwrap();
        let requests = mock.requests();
        mock.shutdown().await;
        drop(rt);
        pool.close().await;
        assert_no_sql(listener);
        if matches!(case, "foreign guild" | "foreign application" | "dm") {
            assert!(!handled, "{case} must not route");
            assert!(requests.is_empty());
        } else {
            assert!(handled, "{case} must be privately refused");
            let reply = private_result(&requests);
            let text = reply["content"].as_str().unwrap();
            if matches!(case, "permission" | "missing permissions") {
                assert_eq!(text, MANAGE_EVENTS_REQUIRED);
            }
            if case == "disabled" {
                assert_eq!(text, ANNOUNCEMENTS_DISABLED_REPLY);
            }
            assert!(!text.contains("Preview — nothing posted"));
        }
    }
}

#[tokio::test]
#[ignore = "needs TWO_TEST_DATABASE_URL disposable database"]
async fn preview_omitted_and_false_each_publish_once_with_real_renderer_and_audit() {
    let db = durable_select_db().await;
    let mock = rest().await;
    let rt = runtime(db.pool().clone(), &mock, true);
    let mut handled = Vec::new();
    for (id, options) in [(9501, vec![]), (9502, vec![flag(false)])] {
        handled.push(
            rt.handle(&command(
                id,
                "Friday raid",
                START,
                "tank:Tank:2,heal:Healer:1",
                &options,
            ))
            .await,
        );
    }
    let rows = select_snapshot(db.pool()).await;
    let requests = mock.requests();
    mock.shutdown().await;
    drop(rt);
    db.close().await.unwrap();
    for result in handled {
        assert!(result.unwrap());
    }
    assert_eq!(rows["posts"].as_array().unwrap().len(), 2);
    assert_eq!(rows["roles"].as_array().unwrap().len(), 4);
    assert_eq!(rows["signups"], json!([]));
    assert_eq!(rows["audits"].as_array().unwrap().len(), 2);
    let posts: Vec<_> = requests
        .iter()
        .filter(|r| r.method == "POST" && r.path == "/api/v10/channels/4444/messages")
        .collect();
    assert_eq!(posts.len(), 2, "one real publication per invocation");
    for (n, post) in posts.into_iter().enumerate() {
        let payload = body(post);
        assert_eq!(payload["allowed_mentions"]["parse"], json!([]));
        assert_eq!(payload["components"][0]["components"][0]["type"], 3);
        assert_eq!(payload["enforce_nonce"], true);
        assert_eq!(
            payload["nonce"],
            lfg::lfg_nonce(&format!("lfg-{}", 9501 + n))
        );
        let text = payload["content"].as_str().unwrap();
        assert!(text.contains("**Tank** 0/2") && text.contains("**Healer** 0/1"));
        assert!(!text.contains("Preview — nothing posted"));
    }
}

#[tokio::test]
#[ignore = "needs TWO_TEST_DATABASE_URL disposable database"]
async fn preview_at_open_post_quota_changes_no_rows_or_audits_and_reserves_no_slot() {
    let db = durable_select_db().await;
    for n in 0..20 {
        let post = lfg::LfgPost {
            id: format!("preview-quota-{n}"),
            guild_id: GUILD.into(),
            channel_id: CHANNEL.into(),
            message_id: None,
            title: "Existing raid".into(),
            starts_at: START.into(),
            status: lfg::LfgStatus::Open,
            created_by: "3333".into(),
            created_at: "2026-10-01T00:00:00Z".into(),
            closed_at: None,
        };
        let roles = lfg::spec_roles(&post.id, &lfg::parse_role_spec("tank:Tank:2").unwrap());
        store::put_lfg(db.pool(), &post, &roles, true)
            .await
            .unwrap();
    }
    let before = select_snapshot(db.pool()).await;
    let mock = rest().await;
    let rt = runtime(db.pool().clone(), &mock, true);
    rt.set_bot_user_id(0);
    let handled = rt
        .handle(&command(
            9601,
            "New raid",
            START,
            "tank:Tank:2",
            &[flag(true)],
        ))
        .await;
    let after = select_snapshot(db.pool()).await;
    let requests = mock.requests();
    mock.shutdown().await;
    drop(rt);
    db.close().await.unwrap();
    assert!(handled.unwrap());
    assert_eq!(
        before["posts"].as_array().unwrap().len(),
        20,
        "quota is actually full"
    );
    assert_eq!(
        after, before,
        "preview must not reserve, persist, sign up, or audit"
    );
    assert_preview(&private_result(&requests), 9601);
}
